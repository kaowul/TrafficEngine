//use e2d2::operators::{ReceiveBatch, Batch, merge, merge_with_selector};
use e2d2::operators::{ReceiveBatch, Batch, merge_auto, SchedulingPolicy};
use e2d2::scheduler::{Runnable, Scheduler, StandaloneScheduler};
use e2d2::allocators::CacheAligned;
use e2d2::headers::{IpHeader, MacHeader, TcpHeader};
use e2d2::interface::*;
use e2d2::queues::{new_mpsc_queue_pair, new_mpsc_queue_pair_with_size};
use e2d2::headers::EndOffset;
use e2d2::utils;
use e2d2::native::zcsi::ipv4_phdr_chksum;

use std::sync::mpsc::{Sender, channel};
use std::sync::Arc;
use std::net::{SocketAddrV4, Ipv4Addr};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::mem;

use uuid::Uuid;
//use serde_json;
use bincode::{serialize, deserialize};
use separator::Separatable;

use netfcts::tcp_common::{TcpState, TcpStatistics, TcpCounter, TcpRole, CData, L234Data, ReleaseCause};
use cmanager::{Connection, ConnectionManagerC, ConnectionManagerS};
use EngineConfig;
use netfcts::system::SystemData;
#[cfg(feature = "profiling")]
use netfcts::TimeAdder;
use netfcts::is_kni_core;
use {PipelineId, MessageFrom, MessageTo, TaskType, Timeouts};
use netfcts::tasks::{PRIVATE_ETYPE_PACKET, PRIVATE_ETYPE_TIMER, ETYPE_IPV4};
use netfcts::tasks::{private_etype, PacketInjector, TickGenerator, install_task};
use netfcts::timer_wheel::TimerWheel;

const MIN_FRAME_SIZE: usize = 60;

pub fn setup_generator(
    core: i32,
    nr_connections: usize, //# of connections to setup per pipeline
    pci: &CacheAligned<PortQueue>,
    kni: &CacheAligned<PortQueue>,
    sched: &mut StandaloneScheduler,
    engine_config: &EngineConfig,
    servers: Vec<L234Data>,
    flowdirector_map: HashMap<i32, Arc<FlowDirector>>,
    tx: Sender<MessageFrom>,
    system_data: SystemData,
) {
    let mut me = engine_config.get_l234data();
    let l4flow_for_this_core = flowdirector_map.get(&pci.port.port_id()).unwrap().get_flow(pci.rxq());
    me.ip = l4flow_for_this_core.ip; // in case we use destination IP address for flow steering

    let pipeline_id = PipelineId {
        core: core as u16,
        port_id: pci.port.port_id() as u16,
        rxq: pci.rxq(),
    };
    debug!("enter setup_generator {}", pipeline_id);

    let mut cm_c = ConnectionManagerC::new(pipeline_id.clone(), pci.clone(), l4flow_for_this_core);
    let mut cm_s = ConnectionManagerS::new();

    let timeouts = Timeouts::default_or_some(&engine_config.timeouts);
    let mut wheel_c = TimerWheel::new(128, system_data.cpu_clock / 10, 128);
    let mut wheel_s = TimerWheel::new(128, system_data.cpu_clock / 10, 128);
    debug!(
        "{} wheel cycle= {} millis",
        pipeline_id,
        wheel_c.get_max_timeout_cycles() / system_data.cpu_clock * 1000
    );

    // setting up a a reverse message channel between this pipeline and the main program thread
    debug!("{} setting up reverse channel", pipeline_id);
    let (remote_tx, rx) = channel::<MessageTo>();
    // we send the transmitter to the remote receiver of our messages
    tx.send(MessageFrom::Channel(pipeline_id.clone(), remote_tx)).unwrap();

    // forwarding frames coming from KNI to PCI, if we are the kni core
    if is_kni_core(pci) {
        let forward2pci = ReceiveBatch::new(kni.clone()).parse::<MacHeader>().send(pci.clone());
        let uuid = Uuid::new_v4();
        let name = String::from("Kni2Pci");
        sched.add_runnable(Runnable::from_task(uuid, name, forward2pci).move_ready());
    }
    let thread_id = format!("<c{}, rx{}>: ", core, pci.rxq());
    let tx_clone = tx.clone();
    let pipeline_id_clone = pipeline_id.clone();
    let mut counter_to = TcpCounter::new();
    let mut counter_from = TcpCounter::new();
    #[cfg(feature = "profiling")]
    let mut rx_tx_stats = Vec::with_capacity(1000);

    // we create SYN and Payload packets and merge them with the upstream coming from the pci i/f
    // the destination port of the created tcp packets is used as discriminator in the pipeline (dst_port 1 is for SYN packet generation)

    let (syn_producer, syn_consumer) = new_mpsc_queue_pair_with_size(64);
    let injector_uuid = install_task(
        sched,
        "SynInjector",
        PacketInjector::new(
            syn_producer,
            &me,
            0,
            system_data.cpu_clock / engine_config.cps_limit() * 32,
            1u16,
        )
        .set_start_delay(system_data.cpu_clock / 100),
    );
    tx.send(MessageFrom::Task(pipeline_id.clone(), injector_uuid, TaskType::TcpGenerator))
        .unwrap();
    let syn_injector_ready_flag = sched.get_ready_flag(&injector_uuid).unwrap();


    let (payload_producer, payload_consumer) = new_mpsc_queue_pair_with_size(64);
    let injector_uuid = install_task(
        sched,
        "PayloadInjector",
        PacketInjector::new(
            payload_producer,
            &me,
            0,
            system_data.cpu_clock / engine_config.cps_limit() * 32,
            2u16,
        )
        .set_start_delay(system_data.cpu_clock / 100),
    );
    tx.send(MessageFrom::Task(pipeline_id.clone(), injector_uuid, TaskType::TcpGenerator))
        .unwrap();
    let payload_injector_ready_flag = sched.get_ready_flag(&injector_uuid).unwrap();

    // set up the generator producing timer tick packets with our private EtherType
    let (producer_timerticks, consumer_timerticks) = new_mpsc_queue_pair();
    let tick_generator = TickGenerator::new(producer_timerticks, &me, system_data.cpu_clock / 100); // 10 ms
    assert!(wheel_c.resolution() > tick_generator.tick_length());
    let wheel_tick_reduction_factor = wheel_c.resolution() / tick_generator.tick_length();
    let mut ticks = 0;
    let uuid_task = install_task(sched, "TickGenerator", tick_generator);
    tx.send(MessageFrom::Task(pipeline_id.clone(), uuid_task, TaskType::TickGenerator))
        .unwrap();

    let receive_pci = ReceiveBatch::new(pci.clone());
    let l2_input_stream = merge_auto(
        vec![
            syn_consumer.compose(),
            payload_consumer.compose(),
            consumer_timerticks.set_urgent().compose(),
            //l2groups.get_group(1).unwrap().compose(),
            receive_pci.compose(),
        ],
        SchedulingPolicy::LongestQueue,
    );

    // group 0 -> dump packets
    // group 1 -> send to PCI
    // group 2 -> send to KNI
    let rxq = pci.rxq();
    let csum_offload = pci.port.csum_offload();
    #[cfg(feature = "profiling")]
    let tx_stats = pci.tx_stats();
    #[cfg(feature = "profiling")]
    let rx_stats = pci.rx_stats();
    let uuid_l4groupby = Uuid::new_v4();
    let uuid_l4groupby_clone = uuid_l4groupby.clone();
    let pipeline_ip = cm_c.ip();
    let tcp_port_base = cm_c.tcp_port_base();

    #[cfg(feature = "profiling")]
    let mut time_adders = [
        TimeAdder::new("cmanager_c", 100000),
        TimeAdder::new("s_recv_syn", 40000),
        TimeAdder::new("s_recv_syn_ack2", 40000),
        TimeAdder::new("s_recv_payload", 40000),
        TimeAdder::new("c_sent_syn", 40000),
        TimeAdder::new("c_sent_payload", 40000),
        TimeAdder::new("c_recv_syn_ack", 40000),
        TimeAdder::new("c_recv_fin", 40000),
        TimeAdder::new("s_recv_fin", 40000),
        TimeAdder::new("c_release_con", 40000),
        TimeAdder::new("s_release_con", 40000),
    ];

    // process TCP traffic addressed to Proxy
    let mut l4groups = l2_input_stream.parse::<MacHeader>().group_by(
        3,
        box move |p_mac| {
            // this is the major closure for TCP processing

            let now = || utils::rdtsc_unsafe().separated_string();

            let _syn_injector_start = || {
                debug!("{} (re-)starting the injector at {}", thread_id, now());
                syn_injector_ready_flag.store(true, Ordering::SeqCst);
            };

            let syn_injector_stop = || {
                debug!("{}: stopping the injector at {}", thread_id, now());
                syn_injector_ready_flag.store(false, Ordering::SeqCst);
            };

            let syn_injector_runs = || syn_injector_ready_flag.load(Ordering::SeqCst);

            let payload_injector_start = || {
                debug!("{} (re-)starting the injector at {}", thread_id, now());
                payload_injector_ready_flag.store(true, Ordering::SeqCst);
            };

            let payload_injector_stop = || {
                debug!("{}: stopping the injector at {}", thread_id, now());
                payload_injector_ready_flag.store(false, Ordering::SeqCst);
            };

            let payload_injector_runs = || payload_injector_ready_flag.load(Ordering::SeqCst);

            struct HeaderState<'a> {
                mac: &'a mut MacHeader,
                ip: &'a mut IpHeader,
                tcp: &'a mut TcpHeader,
            }

            impl<'a> HeaderState<'a> {
                #[inline]
                fn set_server_socket(&mut self, ip: u32, port: u16) {
                    self.ip.set_dst(ip);
                    self.tcp.set_dst_port(port);
                }

                #[inline]
                fn tcp_payload_len(&self) -> usize {
                    self.ip.length() as usize - self.tcp.offset() - self.ip.ihl() as usize * 4
                }
            }

            #[inline]
            fn do_ttl<M: Sized + Send>(h: &mut HeaderState, p: &Packet<TcpHeader, M>) {
                let ttl = h.ip.ttl();
                if ttl >= 1 {
                    h.ip.set_ttl(ttl - 1);
                }
                if !p.tcp_checksum_tx_offload() {
                    h.ip.update_checksum();
                }
            }

            #[inline]
            fn make_reply_packet(h: &mut HeaderState, inc: u32) {
                let smac = h.mac.src;
                let dmac = h.mac.dst;
                let sip = h.ip.src();
                let dip = h.ip.dst();
                let sport = h.tcp.src_port();
                let dport = h.tcp.dst_port();
                h.mac.set_smac(&dmac);
                h.mac.set_dmac(&smac);
                h.ip.set_dst(sip);
                h.ip.set_src(dip);
                h.tcp.set_src_port(dport);
                h.tcp.set_dst_port(sport);
                h.tcp.set_ack_flag();
                let ack_num = h.tcp.seq_num().wrapping_add(h.tcp_payload_len() as u32 + inc);
                h.tcp.set_ack_num(ack_num);
            }

            #[inline]
            fn set_header(server: &L234Data, port: u16, h: &mut HeaderState, me: &L234Data) {
                h.mac.set_dmac(&server.mac);
                h.mac.set_smac(&me.mac);
                h.set_server_socket(server.ip, server.port);
                h.ip.set_src(me.ip);
                h.tcp.set_src_port(port);
            }
            /*
            #[inline]
            pub fn tcpip_payload_size<M: Sized + Send>(p: &Packet<TcpHeader, M>) -> u16 {
                let iph = p.get_pre_header().unwrap();
                // payload size = ip total length - ip header length -tcp header length
                iph.length() - (iph.ihl() as u16) * 4u16 - (p.get_header().data_offset() as u16) * 4u16
            }
            */

            // remove tcp options for SYN and SYN-ACK,
            // pre-requisite: no payload exists, because any payload is not shifted up
            #[inline]
            fn remove_tcp_options<M: Sized + Send>(p: &mut Packet<TcpHeader, M>, h: &mut HeaderState) {
                let old_offset = h.tcp.offset() as u16;
                if old_offset > 20 {
                    debug!("trimming tcp-options by { } bytes", old_offset - 20);
                    h.tcp.set_data_offset(5u8);
                    // minimum mbuf data length is 60 bytes
                    h.ip.trim_length_by(old_offset - 20u16);
                    //                        let trim_by = min(p.data_len() - 60usize, (old_offset - 20u16) as usize);
                    //                        82599 does padding itself !?
                    let trim_by = old_offset - 20;
                    p.trim_payload_size(trim_by as usize);
                }
            }

            #[inline]
            fn syn_received<M: Sized + Send>(p: &mut Packet<TcpHeader, M>, c: &mut Connection, h: &mut HeaderState) {
                c.con_rec.push_state(TcpState::SynReceived);
                c.dut_mac = h.mac.clone();
                c.set_dut_sock((h.ip.src(), h.tcp.src_port()));
                // debug!("checksum in = {:X}",p.get_header().checksum());
                remove_tcp_options(p, h);
                make_reply_packet(h, 1);
                //generate seq number:
                c.seqn_nxt = (utils::rdtsc_unsafe() << 8) as u32;
                h.tcp.set_seq_num(c.seqn_nxt);
                c.ackn_nxt = h.tcp.ack_num();
                c.seqn_nxt = c.seqn_nxt.wrapping_add(1);
                prepare_checksum(p, h);
                trace!("(SYN-)ACK to client, L3: { }, L4: { }", h.ip, h.tcp);
            }

            #[inline]
            fn synack_received<M: Sized + Send>(p: &mut Packet<TcpHeader, M>, c: &mut Connection, h: &mut HeaderState) {
                make_reply_packet(h, 1);
                c.ackn_nxt = h.tcp.ack_num();
                h.tcp.unset_syn_flag();
                h.tcp.set_seq_num(c.seqn_nxt);
                prepare_checksum(p, h);
            }

            #[inline]
            fn strip_payload<M: Sized + Send>(p: &mut Packet<TcpHeader, M>, h: &mut HeaderState) {
                let ip_sz = h.ip.length();
                let payload_len = ip_sz as usize - h.tcp.offset() - h.ip.ihl() as usize * 4;
                h.ip.set_length(ip_sz - payload_len as u16);
                p.trim_payload_size(payload_len);
            }

            #[inline]
            fn s_reply_with_fin<M: Sized + Send>(p: &mut Packet<TcpHeader, M>, c: &mut Connection, h: &mut HeaderState) {
                make_reply_packet(h, 0);
                c.ackn_nxt = h.tcp.ack_num();
                h.tcp.set_seq_num(c.seqn_nxt);
                h.tcp.unset_psh_flag();
                h.tcp.set_fin_flag();
                c.seqn_nxt = c.seqn_nxt.wrapping_add(1);
                strip_payload(p, h);
                prepare_checksum(p, h);
            }

            #[inline]
            fn prepare_checksum<M: Sized + Send>(p: &mut Packet<TcpHeader, M>, h: &mut HeaderState) {
                if p.tcp_checksum_tx_offload() {
                    h.ip.set_csum(0);
                    unsafe {
                        let csum = ipv4_phdr_chksum(h.ip, 0);
                        h.tcp.set_checksum(csum);
                    }
                    p.set_l2_len(mem::size_of::<MacHeader>() as u64);
                    p.set_l3_len(mem::size_of::<IpHeader>() as u64);
                    p.set_l4_len(mem::size_of::<TcpHeader>() as u64);
                //debug!("l234len = {}, {}, {}, ol_flags= 0x{:X}, validate= {}", p.l2_len(), p.l3_len(), p.l4_len(), p.ol_flags(), p.validate_tx_offload() );
                } else {
                    h.ip.update_checksum();
                    update_tcp_checksum(p, h.ip.payload_size(0), h.ip.src(), h.ip.dst());
                    // debug!("checksum recalc = {:X}",p.get_header().checksum());
                }
            }

            #[inline]
            fn generate_syn<M: Sized + Send>(
                p: &mut Packet<TcpHeader, M>,
                c: &mut Connection,
                h: &mut HeaderState,
                me: &L234Data,
                servers: &Vec<L234Data>,
                pipeline_id: &PipelineId,
                syn_counter: &mut usize,
            ) {
                h.mac.set_etype(0x0800); // overwrite private ethertype tag
                c.con_rec.server_index = *syn_counter as usize % servers.len();
                set_header(&servers[c.con_rec.server_index], c.port(), h, me);

                //generate seq number:
                c.seqn_nxt = (utils::rdtsc_unsafe() << 8) as u32;
                h.tcp.set_seq_num(c.seqn_nxt);
                c.seqn_nxt = c.seqn_nxt.wrapping_add(1);
                h.tcp.set_syn_flag();
                h.tcp.set_window_size(5840); // 4* MSS(1460)
                h.tcp.set_ack_num(0u32);
                h.tcp.unset_ack_flag();
                h.tcp.unset_psh_flag();
                prepare_checksum(p, h);

                *syn_counter += 1;
                if *syn_counter % 1000 == 0 {
                    debug!("{}: sent {} SYNs", pipeline_id, *syn_counter);
                }
            }

            #[inline]
            fn make_payload_packet<M: Sized + Send>(
                p: &mut Packet<TcpHeader, M>,
                c: &mut Connection,
                h: &mut HeaderState,
                me: &L234Data,
                servers: &Vec<L234Data>,
                payload: &[u8],
            ) {
                h.mac.set_etype(0x0800); // overwrite private ethertype tag
                set_header(&servers[c.con_rec.server_index], c.port(), h, me);
                let sz = payload.len();
                let ip_sz = h.ip.length();
                p.add_to_payload_tail(sz).expect("insufficient tail room");
                h.ip.set_length(ip_sz + sz as u16);
                h.tcp.set_seq_num(c.seqn_nxt);
                h.tcp.unset_syn_flag();
                h.tcp.set_window_size(5840); // 4* MSS(1460)
                h.tcp.set_ack_num(c.ackn_nxt);
                h.tcp.set_ack_flag();
                h.tcp.set_psh_flag();
                c.seqn_nxt = c.seqn_nxt.wrapping_add(sz as u32);
                p.copy_payload_from_u8_slice(payload);
                if p.data_len() < MIN_FRAME_SIZE {
                    let n_padding_bytes = MIN_FRAME_SIZE - p.data_len();
                    debug!("padding with {} 0x0 bytes", n_padding_bytes);
                    p.add_padding(n_padding_bytes);
                }
                prepare_checksum(p, h);
            }

            #[inline]
            fn passive_close<M: Sized + Send>(
                p: &mut Packet<TcpHeader, M>,
                c: &mut Connection,
                h: &mut HeaderState,
                thread_id: &String,
                counter: &mut TcpCounter,
            ) {
                debug!(
                    "{} passive close on src/dst-port {}/{} in state {:?}",
                    thread_id,
                    c.port(),
                    h.tcp.src_port(),
                    c.con_rec.last_state(),
                );
                c.con_rec.released(ReleaseCause::PassiveClose);
                counter[TcpStatistics::RecvFin] += 1;
                c.con_rec.push_state(TcpState::LastAck);
                make_reply_packet(h, 1);
                c.ackn_nxt = h.tcp.ack_num();
                h.tcp.set_ack_flag();
                h.tcp.set_seq_num(c.seqn_nxt);
                c.seqn_nxt = c.seqn_nxt.wrapping_add(1);
                prepare_checksum(p, h);
                counter[TcpStatistics::SentFinAck] += 1;
            };

            #[inline]
            fn active_close<M: Sized + Send>(
                p: &mut Packet<TcpHeader, M>,
                c: &mut Connection,
                h: &mut HeaderState,
                counter: &mut TcpCounter,
                state: &TcpState,
            ) -> bool {
                let mut tcp_closed = false;
                if h.tcp.ack_flag() && h.tcp.ack_num() == c.seqn_nxt {
                    // we got a FIN+ACK as a receipt to a sent FIN (engine closed connection)
                    debug!(
                        "active close: received FIN+ACK-reply from DUT {:?}:{:?}",
                        h.ip.src(),
                        h.tcp.src_port()
                    );
                    counter[TcpStatistics::RecvFinAck] += 1;
                    c.con_rec.push_state(TcpState::Closed);
                    tcp_closed = true;
                } else {
                    // no ACK
                    debug!(
                        "active close: received FIN-reply from DUT {:?}/{:?}",
                        h.ip.src(),
                        h.tcp.src_port()
                    );
                    counter[TcpStatistics::RecvFinAck] += 1;
                    if *state == TcpState::FinWait1 {
                        c.con_rec.push_state(TcpState::Closing);
                    } else if *state == TcpState::FinWait2 {
                        c.con_rec.push_state(TcpState::Closed);
                        tcp_closed = true
                    }
                }
                make_reply_packet(h, 1);
                h.tcp.unset_fin_flag();
                h.tcp.set_ack_flag();
                c.ackn_nxt = h.tcp.ack_num();
                h.tcp.set_seq_num(c.seqn_nxt);
                if h.tcp_payload_len() > 0 {
                    strip_payload(p, h);
                }
                prepare_checksum(p, h);
                counter[TcpStatistics::SentFinAck2] += 1;
                tcp_closed
            };

            // *****  the closure starts here with processing

            #[cfg(feature = "profiling")]
            let timestamp_entry = utils::rdtsc_unsafe();
            // we must operate on a clone of the borrowed packet, as we want to move it.
            // we release the clone within this closure, we do not care about mbuf refcount
            let p_mac = p_mac.clone_without_ref_counting();
            let b_private_etype = private_etype(&p_mac.get_header().etype());
            if !b_private_etype {
                let header = p_mac.get_header();
                if header.dst != me.mac && !header.dst.is_multicast() && !header.dst.is_broadcast() {
                    debug!("{} from pci: discarding because mac unknown: {} ", thread_id, &header);
                    return 0;
                }
                if header.etype() != 0x0800 && !b_private_etype {
                    // everything other than Ipv4 or our own packets we send to KNI, i.e. group 2
                    return 2;
                }
            }
            let p_ip = p_mac.parse_header::<IpHeader>();
            if !b_private_etype {
                let iph = p_ip.get_header();
                // everything other than TCP, and everything not addressed to us we send to KNI, i.e. group 2
                if iph.protocol() != 6 || iph.dst() != pipeline_ip && iph.dst() != me.ip {
                    return 2;
                }
            }
            let p = &mut p_ip.parse_header::<TcpHeader>();
            let mut group_index = 0usize; // the index of the group to be returned, default 0: dump packet
            if csum_offload {
                p.set_tcp_ipv4_checksum_tx_offload();
            }

            // converting to raw pointer avoids to borrow mutably from p
            let mut hs = HeaderState {
                ip: unsafe { &mut *(p.get_mut_pre_header().unwrap() as *mut IpHeader) },
                mac: unsafe { &mut *(p.get_mut_pre_pre_header().unwrap() as *mut MacHeader) },
                tcp: unsafe { &mut *(p.get_mut_header() as *mut TcpHeader) },
            };

            let src_sock = (hs.ip.src(), hs.tcp.src_port());

            //check ports
            if !b_private_etype && hs.tcp.dst_port() != me.port && hs.tcp.dst_port() < tcp_port_base {
                return 2;
            }

            // if set by the following tcp state machine,
            // the port/connection becomes released/ready afterwards
            // this is cumbersome, but we must make the  borrow checker happy
            let mut release_connection_c = None;
            let mut b_release_connection_s = false;
            let mut ready_connection = None;
            let server_listen_port = cm_c.special_port();

            // check if we got a packet from generator
            match (hs.mac.etype(), hs.tcp.dst_port()) {
                // SYN injection
                (PRIVATE_ETYPE_PACKET, 1) => {
                    if counter_to[TcpStatistics::SentSyn] < nr_connections {
                        if let Some(c) = cm_c.create(TcpRole::Client) {
                            generate_syn(
                                p,
                                c,
                                &mut hs,
                                &me,
                                &servers,
                                &pipeline_id_clone,
                                &mut counter_to[TcpStatistics::SentSyn],
                            );
                            c.con_rec.push_state(TcpState::SynSent);
                            wheel_c.schedule(&(timeouts.established.unwrap() * system_data.cpu_clock / 1000), c.port());
                            group_index = 1;
                            #[cfg(feature = "profiling")]
                            time_adders[4].add(utils::rdtsc_unsafe() - timestamp_entry);
                        }
                    } else {
                        if syn_injector_runs() {
                            syn_injector_stop();
                        }
                    }
                }
                // payload injection
                (PRIVATE_ETYPE_PACKET, 2) => {
                    let mut cdata = CData::new(SocketAddrV4::new(Ipv4Addr::from(cm_c.ip()), cm_c.special_port()), 0, None);
                    if let Some(c) = cm_c.get_ready_connection() {
                        cdata.client_port = c.port();
                        cdata.uuid = *c.get_uuid();
                        let bin_vec = serialize(&cdata).unwrap();
                        make_payload_packet(p, c, &mut hs, &me, &servers, &bin_vec);
                        counter_to[TcpStatistics::Payload] += 1;
                        group_index = 1;
                        #[cfg(feature = "profiling")]
                        time_adders[5].add(utils::rdtsc_unsafe() - timestamp_entry);
                    } else {
                        if payload_injector_runs() {
                            payload_injector_stop();
                        }
                    }
                }
                (PRIVATE_ETYPE_PACKET, _) => {
                    error!("received unknown dst port from PacketInjector");
                }
                (PRIVATE_ETYPE_TIMER, _) => {
                    ticks += 1;
                    match rx.try_recv() {
                        Ok(MessageTo::FetchCounter) => {
                            #[cfg(feature = "profiling")]
                            tx_clone
                                .send(MessageFrom::Counter(
                                    pipeline_id_clone.clone(),
                                    counter_to.clone(),
                                    counter_from.clone(),
                                    Some(rx_tx_stats.clone()),
                                ))
                                .unwrap();
                            #[cfg(not (feature = "profiling"))]
                                tx_clone
                                .send(MessageFrom::Counter(
                                    pipeline_id_clone.clone(),
                                    counter_to.clone(),
                                    counter_from.clone(),
                                    None,
                                ))
                                .unwrap();
                        }
                        Ok(MessageTo::FetchCRecords) => {
                            cm_c.record_uncompleted();
                            cm_s.record_uncompleted();
                            tx_clone
                                .send(MessageFrom::CRecords(
                                    pipeline_id_clone.clone(),
                                    cm_c.fetch_c_records(),
                                    cm_s.fetch_c_records(),
                                ))
                                .unwrap();
                        }
                        _ => {}
                    }
                    // check for timeouts
                    if ticks % wheel_tick_reduction_factor == 0 {
                        cm_c.release_timeouts(&utils::rdtsc_unsafe(), &mut wheel_c);
                        cm_s.release_timeouts(&utils::rdtsc_unsafe(), &mut wheel_s);
                    }
                    #[cfg(feature = "profiling")]
                    rx_tx_stats.push((
                        utils::rdtsc_unsafe(),
                        rx_stats.stats.load(Ordering::Relaxed),
                        tx_stats.stats.load(Ordering::Relaxed),
                    ));
                }
                (ETYPE_IPV4, dst_port) if dst_port == server_listen_port => {
                    //server side
                    let mut opt_c = if hs.tcp.syn_flag() {
                        debug!("got SYN on special port 0x{:x}", cm_c.special_port());
                        counter_from[TcpStatistics::RecvSyn] += 1;
                        let c = cm_s.get_mut_or_insert(&src_sock);
                        c
                    } else {
                        cm_s.get_mut(&src_sock)
                    };

                    match opt_c {
                        None => warn!("no state for this packet on server port"),
                        Some(mut c) => {
                            let old_s_state = c.con_rec.last_state().clone();

                            //check seqn
                            if old_s_state != TcpState::Listen && hs.tcp.seq_num() != c.ackn_nxt {
                                let diff = hs.tcp.seq_num() as i64 - c.ackn_nxt as i64;
                                if diff > 0 {
                                    warn!(
                                        "{} unexpected seqn (packet loss?) in state {:?}, seqn differs by {}",
                                        thread_id, old_s_state, diff
                                    );
                                } else {
                                    debug!("{} state= {:?}, diff= {}, tcp= {}", thread_id, old_s_state, diff, hs.tcp);
                                }
                            } else if hs.tcp.syn_flag() {
                                // check flags
                                if old_s_state == TcpState::Listen {
                                    // replies with a SYN-ACK to client:
                                    syn_received(p, c, &mut hs);
                                    c.con_rec.server_index = rxq as usize; // we misuse this field for the queue number
                                    wheel_s.schedule(
                                        &(timeouts.established.unwrap() * system_data.cpu_clock / 1000),
                                        *c.get_dut_sock().unwrap(),
                                    );
                                    counter_from[TcpStatistics::SentSynAck] += 1;
                                    group_index = 1;
                                    #[cfg(feature = "profiling")]
                                    time_adders[1].add(utils::rdtsc_unsafe() - timestamp_entry);
                                } else {
                                    warn!("{} received SYN in state {:?}", thread_id, c.con_rec.states());
                                }
                            } else if hs.tcp.fin_flag() {
                                trace!("received FIN");
                                if old_s_state >= TcpState::FinWait1 {
                                    if active_close(p, c, &mut hs, &mut counter_from, &old_s_state) {
                                        b_release_connection_s = true;
                                    }
                                    group_index = 1;
                                } else {
                                    // DUT wants to close connection
                                    passive_close(p, c, &mut hs, &thread_id, &mut counter_from);
                                    group_index = 1;
                                }
                                #[cfg(feature = "profiling")]
                                time_adders[8].add(utils::rdtsc_unsafe() - timestamp_entry);
                            } else if hs.tcp.rst_flag() {
                                trace!("received RST");
                                counter_from[TcpStatistics::RecvRst] += 1;
                                c.con_rec.push_state(TcpState::Closed);
                                c.con_rec.released(ReleaseCause::PassiveRst);
                                // release connection in the next block
                                b_release_connection_s = true;
                            } else if hs.tcp.ack_flag() && hs.tcp.ack_num() == c.seqn_nxt {
                                match old_s_state {
                                    TcpState::SynReceived => {
                                        c.con_established();
                                        counter_from[TcpStatistics::RecvSynAck2] += 1;
                                        debug!("{} connection from DUT ({:?}) established", thread_id, src_sock);
                                        #[cfg(feature = "profiling")]
                                        time_adders[2].add(utils::rdtsc_unsafe() - timestamp_entry);
                                    }
                                    TcpState::LastAck => {
                                        // received final ack in passive close
                                        trace!("received final ACK in passive close");
                                        counter_from[TcpStatistics::RecvFinAck2] += 1;
                                        c.con_rec.push_state(TcpState::Closed);
                                        c.con_rec.released(ReleaseCause::PassiveClose);
                                        // release connection in the next block
                                        b_release_connection_s = true;
                                    }
                                    TcpState::FinWait1 => c.con_rec.push_state(TcpState::FinWait2),
                                    TcpState::Closing => {
                                        c.con_rec.push_state(TcpState::Closed);
                                        b_release_connection_s = true;
                                    }
                                    _ => (),
                                }
                            }

                            // process payload
                            if old_s_state >= TcpState::Established && hs.tcp_payload_len() > 0 {
                                if c.con_rec.payload_packets == 0 {
                                    //first payload packet
                                    match deserialize::<CData>(p.get_payload()) {
                                        Ok(cdata) => {
                                            c.set_port(cdata.client_port);
                                            let uuid = cdata.uuid;
                                            debug!("{} received payload {:?}, uuid= {}", thread_id, cdata, uuid.unwrap());
                                            c.set_uuid(uuid);
                                        }
                                        _ => (),
                                    }
                                    if old_s_state == TcpState::Established {
                                        s_reply_with_fin(p, &mut c, &mut hs);
                                        counter_from[TcpStatistics::SentFin] += 1;
                                        c.con_rec.released(ReleaseCause::ActiveClose);
                                        c.con_rec.push_state(TcpState::FinWait1);
                                    }
                                    counter_from[TcpStatistics::Payload] += 1;
                                    c.con_rec.payload_packets += 1;
                                    group_index = 1;
                                } else {
                                    c.ackn_nxt = hs.tcp.seq_num().wrapping_add(hs.tcp_payload_len() as u32);
                                }
                                #[cfg(feature = "profiling")]
                                time_adders[3].add(utils::rdtsc_unsafe() - timestamp_entry);
                            }
                        }
                    }
                }
                (ETYPE_IPV4, client_port) => {
                    // client side
                    // check that flow steering worked:
                    if !cm_c.owns_tcp_port(client_port) {
                        error!("flow steering failed {}", hs.tcp);
                        assert!(cm_c.owns_tcp_port(hs.tcp.dst_port()));
                    }
                    let mut c = cm_c.get_mut_by_port(hs.tcp.dst_port());
                    #[cfg(feature = "profiling")]
                    time_adders[0].add(utils::rdtsc_unsafe() - timestamp_entry);
                    if c.is_some() {
                        let mut c = c.as_mut().unwrap();
                        //debug!("incoming packet for connection {}", c);
                        let old_c_state = c.con_rec.last_state().clone();

                        //check seqn
                        if old_c_state != TcpState::SynSent && hs.tcp.seq_num() != c.ackn_nxt {
                            let diff = hs.tcp.seq_num() as i64 - c.ackn_nxt as i64;
                            if diff > 0 {
                                warn!(
                                    "{} unexpected sequence number (packet loss?) in state {:?}, seqn differs by {}",
                                    thread_id, old_c_state, diff
                                );
                            } else {
                                debug!("{} state= {:?}, diff= {}, tcp= {}", thread_id, old_c_state, diff, hs.tcp);
                            }
                        } else if hs.tcp.ack_flag() && hs.tcp.syn_flag() {
                            group_index = 1;
                            counter_to[TcpStatistics::RecvSynAck] += 1;
                            if old_c_state == TcpState::SynSent {
                                c.con_established();
                                ready_connection = Some(c.port());
                                debug!(
                                    "{} connection for port {} to DUT ({:?}) established ",
                                    thread_id,
                                    c.port(),
                                    src_sock
                                );
                                synack_received(p, &mut c, &mut hs);
                                counter_to[TcpStatistics::SentSynAck2] += 1;
                            } else if old_c_state == TcpState::Established {
                                synack_received(p, &mut c, &mut hs);
                                counter_to[TcpStatistics::SentSynAck2] += 1;
                            } else {
                                warn!("{} received SYN-ACK in wrong state: {:?}", thread_id, old_c_state);
                                group_index = 0;
                            } // ignore the SynAck
                        } else if hs.tcp.fin_flag() {
                            if old_c_state >= TcpState::FinWait1 {
                                active_close(p, c, &mut hs, &mut counter_to, &old_c_state);
                                group_index = 1;
                            } else {
                                passive_close(p, c, &mut hs, &thread_id, &mut counter_to);
                                group_index = 1;
                            }
                            #[cfg(feature = "profiling")]
                            time_adders[7].add(utils::rdtsc_unsafe() - timestamp_entry);
                        } else if hs.tcp.rst_flag() {
                            counter_to[TcpStatistics::RecvRst] += 1;
                            c.con_rec.push_state(TcpState::Closed);
                            c.con_rec.released(ReleaseCause::PassiveRst);
                            // release connection in the next block
                            release_connection_c = Some(c.port());
                        } else if old_c_state == TcpState::LastAck && hs.tcp.ack_flag() && hs.tcp.ack_num() == c.seqn_nxt {
                            counter_to[TcpStatistics::RecvFinAck2] += 1;
                            c.con_rec.push_state(TcpState::Closed);
                            c.con_rec.released(ReleaseCause::PassiveClose);
                            // release connection in the next block
                            release_connection_c = Some(c.port());
                        } else if hs.tcp.ack_flag() {
                            // ACKs to payload packets
                        } else {
                            counter_to[TcpStatistics::Unexpected] += 1;
                            warn!(
                                "{} unexpected TCP packet on port {} in client state {:?}, sending to KNI i/f: {}",
                                thread_id,
                                hs.tcp.dst_port(),
                                c.con_rec.states(),
                                hs.tcp,
                            );
                            group_index = 2;
                        }
                    } else {
                        warn!(
                            "{} engine has no state for {}:{}, sending to KNI i/f",
                            thread_id,
                            hs.ip,
                            hs.tcp,
                            //Ipv4Addr::from(hs.ip.dst()),
                            // hs.tcp.dst_port(),
                        );
                        // we send this to KNI which handles out-of-order TCP, e.g. by sending RST
                        group_index = 2;
                    }
                }
                (_, _) => assert!(false), // should never happen
            }

            // here we check if we shall release the connection state,
            // need this cumbersome way because of borrow checker for the connection managers
            if b_release_connection_s {
                cm_s.release_sock(&src_sock);
                #[cfg(feature = "profiling")]
                time_adders[10].add(utils::rdtsc_unsafe() - timestamp_entry);
            }
            if let Some(sport) = release_connection_c {
                //debug!("releasing port {}", sport);
                cm_c.release_port(sport);
                #[cfg(feature = "profiling")]
                time_adders[9].add(utils::rdtsc_unsafe() - timestamp_entry);
            }
            if let Some(sport) = ready_connection {
                trace!("{} connection on  port {} is ready", thread_id, sport);
                cm_c.set_ready_connection(sport);
                // if this is the first ready connection, we restart the injector, avoid accessing Atomic unnecessarily
                if cm_c.ready_connections() == 1 {
                    payload_injector_start();
                }
                #[cfg(feature = "profiling")]
                time_adders[6].add(utils::rdtsc_unsafe() - timestamp_entry);
            }
            do_ttl(&mut hs, &p);
            group_index
        },
        sched,
        "L4-Groups".to_string(),
        uuid_l4groupby_clone,
    );

    let pipe2kni = l4groups.get_group(2).unwrap().send(kni.clone());
    let l4pciflow = l4groups.get_group(1).unwrap().compose();
    let l4dumpflow = l4groups.get_group(0).unwrap().filter(box move |_| false).compose();
    let pipe2pci = merge_auto(vec![l4pciflow, l4dumpflow], SchedulingPolicy::LongestQueue).send(pci.clone());

    let uuid_pipe2kni = install_task(sched, "Pipe2Kni", pipe2kni);
    tx.send(MessageFrom::Task(pipeline_id.clone(), uuid_pipe2kni, TaskType::Pipe2Kni))
        .unwrap();

    let uuid_pipe2pic = install_task(sched, "Pipe2Pci", pipe2pci);
    tx.send(MessageFrom::Task(pipeline_id.clone(), uuid_pipe2pic, TaskType::Pipe2Pci))
        .unwrap();
}
