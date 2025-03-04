use std::{
    collections::BTreeSet,
    fmt::Debug,
    net::Ipv4Addr,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Weak,
    },
    time::{Duration, SystemTime},
};

use dashmap::DashMap;
use petgraph::{
    algo::{all_simple_paths, astar, dijkstra},
    graph::NodeIndex,
    Directed, Graph,
};
use serde::{Deserialize, Serialize};
use tokio::{
    select,
    sync::Mutex,
    task::{JoinHandle, JoinSet},
};

use crate::{
    common::{global_ctx::ArcGlobalCtx, stun::StunInfoCollectorTrait, PeerId},
    peers::route_trait::{Route, RouteInterfaceBox},
    rpc::{NatType, StunInfo},
};

use super::{
    peer_rpc::PeerRpcManager,
    route_trait::{
        DefaultRouteCostCalculator, NextHopPolicy, RouteCostCalculator,
        RouteCostCalculatorInterface,
    },
    PeerPacketFilter,
};

static SERVICE_ID: u32 = 7;
static UPDATE_PEER_INFO_PERIOD: Duration = Duration::from_secs(3600);
static REMOVE_DEAD_PEER_INFO_AFTER: Duration = Duration::from_secs(3660);

type Version = u32;

#[derive(Debug, Clone)]
struct AtomicVersion(Arc<AtomicU32>);

impl AtomicVersion {
    fn new() -> Self {
        AtomicVersion(Arc::new(AtomicU32::new(0)))
    }

    fn get(&self) -> Version {
        self.0.load(Ordering::Relaxed)
    }

    fn set(&self, version: Version) {
        self.0.store(version, Ordering::Relaxed);
    }

    fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn set_if_larger(&self, version: Version) {
        if self.get() < version {
            self.set(version);
        }
    }
}

impl From<Version> for AtomicVersion {
    fn from(version: Version) -> Self {
        AtomicVersion(Arc::new(AtomicU32::new(version)))
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
struct RoutePeerInfo {
    // means next hop in route table.
    peer_id: PeerId,
    inst_id: uuid::Uuid,
    cost: u8,
    ipv4_addr: Option<Ipv4Addr>,
    proxy_cidrs: Vec<String>,
    hostname: Option<String>,
    udp_stun_info: i8,
    last_update: SystemTime,
    version: Version,
}

impl RoutePeerInfo {
    pub fn new() -> Self {
        Self {
            peer_id: 0,
            inst_id: uuid::Uuid::nil(),
            cost: 0,
            ipv4_addr: None,
            proxy_cidrs: Vec::new(),
            hostname: None,
            udp_stun_info: 0,
            last_update: SystemTime::now(),
            version: 0,
        }
    }

    pub fn update_self(&self, my_peer_id: PeerId, global_ctx: &ArcGlobalCtx) -> Self {
        let mut new = Self {
            peer_id: my_peer_id,
            inst_id: global_ctx.get_id(),
            cost: 0,
            ipv4_addr: global_ctx.get_ipv4(),
            proxy_cidrs: global_ctx
                .get_proxy_cidrs()
                .iter()
                .map(|x| x.to_string())
                .chain(global_ctx.get_vpn_portal_cidr().map(|x| x.to_string()))
                .collect(),
            hostname: Some(global_ctx.get_hostname()),
            udp_stun_info: global_ctx
                .get_stun_info_collector()
                .get_stun_info()
                .udp_nat_type as i8,
            // following fields do not participate in comparison.
            last_update: self.last_update,
            version: self.version,
        };

        let need_update_periodically = if let Ok(d) = new.last_update.elapsed() {
            d > UPDATE_PEER_INFO_PERIOD
        } else {
            true
        };

        if new != *self || need_update_periodically {
            new.last_update = SystemTime::now();
            new.version += 1;
        }

        new
    }
}

impl Into<crate::rpc::Route> for RoutePeerInfo {
    fn into(self) -> crate::rpc::Route {
        crate::rpc::Route {
            peer_id: self.peer_id,
            ipv4_addr: if let Some(ipv4_addr) = self.ipv4_addr {
                ipv4_addr.to_string()
            } else {
                "".to_string()
            },
            next_hop_peer_id: 0,
            cost: self.cost as i32,
            proxy_cidrs: self.proxy_cidrs.clone(),
            hostname: self.hostname.unwrap_or_default(),
            stun_info: {
                let mut stun_info = StunInfo::default();
                if let Ok(udp_nat_type) = NatType::try_from(self.udp_stun_info as i32) {
                    stun_info.set_udp_nat_type(udp_nat_type);
                }
                Some(stun_info)
            },
            inst_id: self.inst_id.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
struct RouteConnBitmap {
    peer_ids: Vec<(PeerId, Version)>,
    bitmap: Vec<u8>,
}

impl RouteConnBitmap {
    fn new() -> Self {
        RouteConnBitmap {
            peer_ids: Vec::new(),
            bitmap: Vec::new(),
        }
    }

    fn get_bit(&self, idx: usize) -> bool {
        let byte_idx = idx / 8;
        let bit_idx = idx % 8;
        let byte = self.bitmap[byte_idx];
        (byte >> bit_idx) & 1 == 1
    }

    fn get_connected_peers(&self, peer_idx: usize) -> BTreeSet<PeerId> {
        let mut connected_peers = BTreeSet::new();
        for (idx, (peer_id, _)) in self.peer_ids.iter().enumerate() {
            if self.get_bit(peer_idx * self.peer_ids.len() + idx) {
                connected_peers.insert(*peer_id);
            }
        }
        connected_peers
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
enum Error {
    DuplicatePeerId,
    Stopped,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SyncRouteInfoResponse {
    is_initiator: bool,
    session_id: SessionId,
}

#[tarpc::service]
trait RouteService {
    async fn sync_route_info(
        my_peer_id: PeerId,
        my_session_id: SessionId,
        is_initiator: bool,
        peer_infos: Option<Vec<RoutePeerInfo>>,
        conn_bitmap: Option<RouteConnBitmap>,
    ) -> Result<SyncRouteInfoResponse, Error>;
}

// constructed with all infos synced from all peers.
#[derive(Debug)]
struct SyncedRouteInfo {
    peer_infos: DashMap<PeerId, RoutePeerInfo>,
    conn_map: DashMap<PeerId, (BTreeSet<PeerId>, AtomicVersion)>,
}

impl SyncedRouteInfo {
    fn get_connected_peers<T: FromIterator<PeerId>>(&self, peer_id: PeerId) -> Option<T> {
        self.conn_map
            .get(&peer_id)
            .map(|x| x.0.clone().iter().map(|x| *x).collect())
    }

    fn remove_peer(&self, peer_id: PeerId) {
        tracing::warn!(?peer_id, "remove_peer from synced_route_info");
        self.peer_infos.remove(&peer_id);
        self.conn_map.remove(&peer_id);
    }

    fn fill_empty_peer_info(&self, peer_ids: &BTreeSet<PeerId>) {
        for peer_id in peer_ids {
            self.peer_infos
                .entry(*peer_id)
                .or_insert_with(|| RoutePeerInfo::new());

            self.conn_map
                .entry(*peer_id)
                .or_insert_with(|| (BTreeSet::new(), AtomicVersion::new()));
        }
    }

    fn get_peer_info_version_with_default(&self, peer_id: PeerId) -> Version {
        self.peer_infos
            .get(&peer_id)
            .map(|x| x.version)
            .unwrap_or(0)
    }

    fn check_duplicate_peer_id(
        &self,
        my_peer_id: PeerId,
        dst_peer_id: PeerId,
        route_infos: &Vec<RoutePeerInfo>,
    ) -> Result<(), Error> {
        // 1. check if we are duplicated.
        for info in route_infos.iter() {
            if info.peer_id == my_peer_id {
                if info.version > self.get_peer_info_version_with_default(info.peer_id) {
                    // if dst peer send to us with higher version info of my peer, our peer id is duplicated
                    // TODO: handle this better. restart peer manager?
                    panic!("my peer id is duplicated");
                    // return Err(Error::DuplicatePeerId);
                }
            }

            if info.peer_id == dst_peer_id {
                if info.version < self.get_peer_info_version_with_default(info.peer_id) {
                    // if dst peer send to us with lower version info of dst peer, dst peer id is duplicated
                    return Err(Error::DuplicatePeerId);
                }
            }
        }
        Ok(())
    }

    fn update_peer_infos(
        &self,
        my_peer_id: PeerId,
        dst_peer_id: PeerId,
        peer_infos: &Vec<RoutePeerInfo>,
    ) -> Result<(), Error> {
        self.check_duplicate_peer_id(my_peer_id, dst_peer_id, peer_infos)?;
        for mut route_info in peer_infos.iter().map(Clone::clone) {
            // time between peers may not be synchronized, so update last_update to local now.
            // note only last_update with larger version will be updated to local saved peer info.
            route_info.last_update = SystemTime::now();

            self.peer_infos
                .entry(route_info.peer_id)
                .and_modify(|old_entry| {
                    if route_info.version > old_entry.version {
                        *old_entry = route_info.clone();
                    }
                })
                .or_insert_with(|| route_info.clone());
        }
        Ok(())
    }

    fn update_conn_map(&self, conn_bitmap: &RouteConnBitmap) {
        self.fill_empty_peer_info(&conn_bitmap.peer_ids.iter().map(|x| x.0).collect());

        for (peer_idx, (peer_id, version)) in conn_bitmap.peer_ids.iter().enumerate() {
            assert!(self.peer_infos.contains_key(peer_id));
            let connceted_peers = conn_bitmap.get_connected_peers(peer_idx);
            self.fill_empty_peer_info(&connceted_peers);

            self.conn_map
                .entry(*peer_id)
                .and_modify(|(old_conn_bitmap, old_version)| {
                    if *version > old_version.get() {
                        *old_conn_bitmap = conn_bitmap.get_connected_peers(peer_idx);
                        old_version.set(*version);
                    }
                })
                .or_insert_with(|| {
                    (
                        conn_bitmap.get_connected_peers(peer_idx),
                        version.clone().into(),
                    )
                });
        }
    }

    fn update_my_peer_info(&self, my_peer_id: PeerId, global_ctx: &ArcGlobalCtx) -> bool {
        let mut old = self
            .peer_infos
            .entry(my_peer_id)
            .or_insert(RoutePeerInfo::new());
        let new = old.update_self(my_peer_id, &global_ctx);
        let new_version = new.version;
        let old_version = old.version;
        *old = new;

        new_version != old_version
    }

    fn update_my_conn_info(&self, my_peer_id: PeerId, connected_peers: BTreeSet<PeerId>) -> bool {
        self.fill_empty_peer_info(&connected_peers);

        let mut my_conn_info = self
            .conn_map
            .entry(my_peer_id)
            .or_insert((BTreeSet::new(), AtomicVersion::new()));

        if connected_peers == my_conn_info.value().0 {
            false
        } else {
            let _ = std::mem::replace(&mut my_conn_info.value_mut().0, connected_peers);
            my_conn_info.value().1.inc();
            true
        }
    }

    fn is_peer_bidirectly_connected(&self, src_peer_id: PeerId, dst_peer_id: PeerId) -> bool {
        self.conn_map
            .get(&src_peer_id)
            .map(|x| x.0.contains(&dst_peer_id))
            .unwrap_or(false)
    }

    fn is_peer_directly_connected(&self, src_peer_id: PeerId, dst_peer_id: PeerId) -> bool {
        return self.is_peer_bidirectly_connected(src_peer_id, dst_peer_id)
            || self.is_peer_bidirectly_connected(dst_peer_id, src_peer_id);
    }
}

type PeerGraph = Graph<PeerId, i32, Directed>;
type PeerIdToNodexIdxMap = DashMap<PeerId, NodeIndex>;
type NextHopMap = DashMap<PeerId, (PeerId, i32)>;

// computed with SyncedRouteInfo. used to get next hop.
#[derive(Debug)]
struct RouteTable {
    peer_infos: DashMap<PeerId, RoutePeerInfo>,
    next_hop_map: NextHopMap,
    ipv4_peer_id_map: DashMap<Ipv4Addr, PeerId>,
    cidr_peer_id_map: DashMap<cidr::IpCidr, PeerId>,
}

impl RouteTable {
    fn new() -> Self {
        RouteTable {
            peer_infos: DashMap::new(),
            next_hop_map: DashMap::new(),
            ipv4_peer_id_map: DashMap::new(),
            cidr_peer_id_map: DashMap::new(),
        }
    }

    fn get_next_hop(&self, dst_peer_id: PeerId) -> Option<(PeerId, i32)> {
        self.next_hop_map.get(&dst_peer_id).map(|x| *x)
    }

    fn peer_reachable(&self, peer_id: PeerId) -> bool {
        self.next_hop_map.contains_key(&peer_id)
    }

    fn get_nat_type(&self, peer_id: PeerId) -> Option<NatType> {
        self.peer_infos
            .get(&peer_id)
            .map(|x| NatType::try_from(x.udp_stun_info as i32).unwrap())
    }

    fn build_peer_graph_from_synced_info<T: RouteCostCalculatorInterface>(
        peers: Vec<PeerId>,
        synced_info: &SyncedRouteInfo,
        cost_calc: &mut T,
    ) -> (PeerGraph, PeerIdToNodexIdxMap) {
        let mut graph: PeerGraph = Graph::new();
        let peer_id_to_node_index = PeerIdToNodexIdxMap::new();
        for peer_id in peers.iter() {
            peer_id_to_node_index.insert(*peer_id, graph.add_node(*peer_id));
        }

        for peer_id in peers.iter() {
            let connected_peers = synced_info
                .get_connected_peers(*peer_id)
                .unwrap_or(BTreeSet::new());
            for dst_peer_id in connected_peers.iter() {
                let Some(dst_idx) = peer_id_to_node_index.get(dst_peer_id) else {
                    continue;
                };

                graph.add_edge(
                    *peer_id_to_node_index.get(&peer_id).unwrap(),
                    *dst_idx,
                    cost_calc.calculate_cost(*peer_id, *dst_peer_id),
                );
            }
        }

        (graph, peer_id_to_node_index)
    }

    fn gen_next_hop_map_with_least_hop<T: RouteCostCalculatorInterface>(
        my_peer_id: PeerId,
        graph: &PeerGraph,
        idx_map: &PeerIdToNodexIdxMap,
        cost_calc: &mut T,
    ) -> NextHopMap {
        let res = dijkstra(&graph, *idx_map.get(&my_peer_id).unwrap(), None, |_| 1);
        let next_hop_map = NextHopMap::new();
        for (node_idx, cost) in res.iter() {
            if *cost == 0 {
                continue;
            }
            let all_paths = all_simple_paths::<Vec<_>, _>(
                graph,
                *idx_map.get(&my_peer_id).unwrap(),
                *node_idx,
                *cost - 1,
                Some(*cost - 1),
            )
            .collect::<Vec<_>>();

            assert!(!all_paths.is_empty());

            // find a path with least cost.
            let mut min_cost = i32::MAX;
            let mut min_path = Vec::new();
            for path in all_paths.iter() {
                let mut cost = 0;
                for i in 0..path.len() - 1 {
                    let src_peer_id = *graph.node_weight(path[i]).unwrap();
                    let dst_peer_id = *graph.node_weight(path[i + 1]).unwrap();
                    cost += cost_calc.calculate_cost(src_peer_id, dst_peer_id);
                }

                if cost <= min_cost {
                    min_cost = cost;
                    min_path = path.clone();
                }
            }
            next_hop_map.insert(
                *graph.node_weight(*node_idx).unwrap(),
                (*graph.node_weight(min_path[1]).unwrap(), *cost as i32),
            );
        }

        next_hop_map
    }

    fn gen_next_hop_map_with_least_cost(
        my_peer_id: PeerId,
        graph: &PeerGraph,
        idx_map: &PeerIdToNodexIdxMap,
    ) -> NextHopMap {
        let next_hop_map = NextHopMap::new();
        for item in idx_map.iter() {
            if *item.key() == my_peer_id {
                continue;
            }

            let dst_peer_node_idx = *item.value();

            let Some((cost, path)) = astar::astar(
                graph,
                *idx_map.get(&my_peer_id).unwrap(),
                |node_idx| node_idx == dst_peer_node_idx,
                |e| *e.weight(),
                |_| 0,
            ) else {
                continue;
            };

            next_hop_map.insert(*item.key(), (*graph.node_weight(path[1]).unwrap(), cost));
        }

        next_hop_map
    }

    fn build_from_synced_info<T: RouteCostCalculatorInterface>(
        &self,
        my_peer_id: PeerId,
        synced_info: &SyncedRouteInfo,
        policy: NextHopPolicy,
        mut cost_calc: T,
    ) {
        // build  peer_infos
        self.peer_infos.clear();
        for item in synced_info.peer_infos.iter() {
            let peer_id = item.key();
            let info = item.value();

            if info.version == 0 {
                continue;
            }

            self.peer_infos.insert(*peer_id, info.clone());
        }

        if self.peer_infos.is_empty() {
            return;
        }

        // build next hop map
        self.next_hop_map.clear();
        self.next_hop_map.insert(my_peer_id, (my_peer_id, 0));
        let (graph, idx_map) = Self::build_peer_graph_from_synced_info(
            self.peer_infos.iter().map(|x| *x.key()).collect(),
            &synced_info,
            &mut cost_calc,
        );
        let next_hop_map = if matches!(policy, NextHopPolicy::LeastHop) {
            Self::gen_next_hop_map_with_least_hop(my_peer_id, &graph, &idx_map, &mut cost_calc)
        } else {
            Self::gen_next_hop_map_with_least_cost(my_peer_id, &graph, &idx_map)
        };
        for item in next_hop_map.iter() {
            self.next_hop_map.insert(*item.key(), *item.value());
        }
        // build graph

        // build ipv4_peer_id_map, cidr_peer_id_map
        self.ipv4_peer_id_map.clear();
        self.cidr_peer_id_map.clear();
        for item in self.peer_infos.iter() {
            // only set ipv4 map for peers we can reach.
            if !self.next_hop_map.contains_key(item.key()) {
                continue;
            }

            let peer_id = item.key();
            let info = item.value();

            if let Some(ipv4_addr) = info.ipv4_addr {
                self.ipv4_peer_id_map.insert(ipv4_addr, *peer_id);
            }

            for cidr in info.proxy_cidrs.iter() {
                self.cidr_peer_id_map
                    .insert(cidr.parse().unwrap(), *peer_id);
            }
        }
    }

    fn get_peer_id_for_proxy(&self, ipv4: &Ipv4Addr) -> Option<PeerId> {
        let ipv4 = std::net::IpAddr::V4(*ipv4);
        for item in self.cidr_peer_id_map.iter() {
            let (k, v) = item.pair();
            if k.contains(&ipv4) {
                return Some(*v);
            }
        }
        None
    }
}

type SessionId = u64;

type AtomicSessionId = atomic_shim::AtomicU64;

struct SessionTask {
    task: Arc<std::sync::Mutex<Option<JoinHandle<()>>>>,
}

impl SessionTask {
    fn new() -> Self {
        SessionTask {
            task: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn set_task(&self, task: JoinHandle<()>) {
        if let Some(old) = self.task.lock().unwrap().replace(task) {
            old.abort();
        }
    }

    fn is_running(&self) -> bool {
        if let Some(task) = self.task.lock().unwrap().as_ref() {
            !task.is_finished()
        } else {
            false
        }
    }
}

impl Drop for SessionTask {
    fn drop(&mut self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }
}

impl Debug for SessionTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTask")
            .field("is_running", &self.is_running())
            .finish()
    }
}

// if we need to sync route info with one peer, we create a SyncRouteSession with that peer.
#[derive(Debug)]
struct SyncRouteSession {
    dst_peer_id: PeerId,
    dst_saved_peer_info_versions: DashMap<PeerId, AtomicVersion>,
    dst_saved_conn_bitmap_version: DashMap<PeerId, AtomicVersion>,

    my_session_id: AtomicSessionId,
    dst_session_id: AtomicSessionId,

    // every node should have exactly one initator session to one other non-initiator peer.
    we_are_initiator: AtomicBool,
    dst_is_initiator: AtomicBool,

    need_sync_initiator_info: AtomicBool,

    rpc_tx_count: AtomicU32,
    rpc_rx_count: AtomicU32,

    task: SessionTask,
}

impl SyncRouteSession {
    fn new(dst_peer_id: PeerId) -> Self {
        SyncRouteSession {
            dst_peer_id,
            dst_saved_peer_info_versions: DashMap::new(),
            dst_saved_conn_bitmap_version: DashMap::new(),

            my_session_id: AtomicSessionId::new(rand::random()),
            dst_session_id: AtomicSessionId::new(0),

            we_are_initiator: AtomicBool::new(false),
            dst_is_initiator: AtomicBool::new(false),

            need_sync_initiator_info: AtomicBool::new(false),

            rpc_tx_count: AtomicU32::new(0),
            rpc_rx_count: AtomicU32::new(0),

            task: SessionTask::new(),
        }
    }

    fn check_saved_peer_info_update_to_date(&self, peer_id: PeerId, version: Version) -> bool {
        if version == 0 || peer_id == self.dst_peer_id {
            // never send version 0 peer info to dst peer.
            return true;
        }
        self.dst_saved_peer_info_versions
            .get(&peer_id)
            .map(|v| v.get() >= version)
            .unwrap_or(false)
    }

    fn update_dst_saved_peer_info_version(&self, infos: &Vec<RoutePeerInfo>) {
        for info in infos.iter() {
            self.dst_saved_peer_info_versions
                .entry(info.peer_id)
                .or_insert_with(|| AtomicVersion::new())
                .set_if_larger(info.version);
        }
    }

    fn update_dst_saved_conn_bitmap_version(&self, conn_bitmap: &RouteConnBitmap) {
        for (peer_id, version) in conn_bitmap.peer_ids.iter() {
            self.dst_saved_conn_bitmap_version
                .entry(*peer_id)
                .or_insert_with(|| AtomicVersion::new())
                .set_if_larger(*version);
        }
    }

    fn update_initiator_flag(&self, is_initiator: bool) {
        self.we_are_initiator.store(is_initiator, Ordering::Relaxed);
        self.need_sync_initiator_info.store(true, Ordering::Relaxed);
    }

    fn update_dst_session_id(&self, session_id: SessionId) {
        if session_id != self.dst_session_id.load(Ordering::Relaxed) {
            tracing::warn!(?self, ?session_id, "session id mismatch, clear saved info.");
            self.dst_session_id.store(session_id, Ordering::Relaxed);
            self.dst_saved_conn_bitmap_version.clear();
            self.dst_saved_peer_info_versions.clear();
        }
    }

    fn short_debug_string(&self) -> String {
        format!(
            "session_dst_peer: {:?}, my_session_id: {:?}, dst_session_id: {:?}, we_are_initiator: {:?}, dst_is_initiator: {:?}, rpc_tx_count: {:?}, rpc_rx_count: {:?}, task: {:?}",
            self.dst_peer_id,
            self.my_session_id,
            self.dst_session_id,
            self.we_are_initiator,
            self.dst_is_initiator,
            self.rpc_tx_count,
            self.rpc_rx_count,
            self.task
        )
    }
}

struct PeerRouteServiceImpl {
    my_peer_id: PeerId,
    global_ctx: ArcGlobalCtx,
    sessions: DashMap<PeerId, Arc<SyncRouteSession>>,

    interface: Arc<Mutex<Option<RouteInterfaceBox>>>,

    cost_calculator: Arc<std::sync::Mutex<Option<RouteCostCalculator>>>,
    route_table: RouteTable,
    route_table_with_cost: RouteTable,
    synced_route_info: Arc<SyncedRouteInfo>,
    cached_local_conn_map: std::sync::Mutex<RouteConnBitmap>,
}

impl Debug for PeerRouteServiceImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerRouteServiceImpl")
            .field("my_peer_id", &self.my_peer_id)
            .field("sessions", &self.sessions)
            .field("route_table", &self.route_table)
            .field("route_table_with_cost", &self.route_table_with_cost)
            .field("synced_route_info", &self.synced_route_info)
            .field(
                "cached_local_conn_map",
                &self.cached_local_conn_map.lock().unwrap(),
            )
            .finish()
    }
}

impl PeerRouteServiceImpl {
    fn new(my_peer_id: PeerId, global_ctx: ArcGlobalCtx) -> Self {
        PeerRouteServiceImpl {
            my_peer_id,
            global_ctx,
            sessions: DashMap::new(),

            interface: Arc::new(Mutex::new(None)),

            cost_calculator: Arc::new(std::sync::Mutex::new(Some(Box::new(
                DefaultRouteCostCalculator,
            )))),

            route_table: RouteTable::new(),
            route_table_with_cost: RouteTable::new(),

            synced_route_info: Arc::new(SyncedRouteInfo {
                peer_infos: DashMap::new(),
                conn_map: DashMap::new(),
            }),
            cached_local_conn_map: std::sync::Mutex::new(RouteConnBitmap::new()),
        }
    }

    fn get_or_create_session(&self, dst_peer_id: PeerId) -> Arc<SyncRouteSession> {
        self.sessions
            .entry(dst_peer_id)
            .or_insert_with(|| Arc::new(SyncRouteSession::new(dst_peer_id)))
            .value()
            .clone()
    }

    fn get_session(&self, dst_peer_id: PeerId) -> Option<Arc<SyncRouteSession>> {
        self.sessions.get(&dst_peer_id).map(|x| x.value().clone())
    }

    fn remove_session(&self, dst_peer_id: PeerId) {
        self.sessions.remove(&dst_peer_id);
    }

    fn list_session_peers(&self) -> Vec<PeerId> {
        self.sessions.iter().map(|x| *x.key()).collect()
    }

    async fn list_peers_from_interface<T: FromIterator<PeerId>>(&self) -> T {
        self.interface
            .lock()
            .await
            .as_ref()
            .unwrap()
            .list_peers()
            .await
            .into_iter()
            .collect()
    }

    fn update_my_peer_info(&self) -> bool {
        if self
            .synced_route_info
            .update_my_peer_info(self.my_peer_id, &self.global_ctx)
        {
            self.update_route_table_and_cached_local_conn_bitmap();
            return true;
        }
        false
    }

    async fn update_my_conn_info(&self) -> bool {
        let connected_peers: BTreeSet<PeerId> = self.list_peers_from_interface().await;
        let updated = self
            .synced_route_info
            .update_my_conn_info(self.my_peer_id, connected_peers);

        if updated {
            self.update_route_table_and_cached_local_conn_bitmap();
        }

        updated
    }

    fn update_route_table(&self) {
        let mut calc_locked = self.cost_calculator.lock().unwrap();

        calc_locked.as_mut().unwrap().begin_update();
        self.route_table.build_from_synced_info(
            self.my_peer_id,
            &self.synced_route_info,
            NextHopPolicy::LeastHop,
            calc_locked.as_mut().unwrap(),
        );

        self.route_table_with_cost.build_from_synced_info(
            self.my_peer_id,
            &self.synced_route_info,
            NextHopPolicy::LeastCost,
            calc_locked.as_mut().unwrap(),
        );
        calc_locked.as_mut().unwrap().end_update();
    }

    fn cost_calculator_need_update(&self) -> bool {
        self.cost_calculator
            .lock()
            .unwrap()
            .as_ref()
            .map(|x| x.need_update())
            .unwrap_or(false)
    }

    fn update_route_table_and_cached_local_conn_bitmap(&self) {
        // update route table first because we want to filter out unreachable peers.
        self.update_route_table();

        // the conn_bitmap should contain complete list of directly connected peers.
        // use union of dst peers can preserve this property.
        let all_dst_peer_ids = self
            .synced_route_info
            .conn_map
            .iter()
            .map(|x| x.value().clone().0.into_iter())
            .flatten()
            .collect::<BTreeSet<_>>();

        let all_peer_ids = self
            .synced_route_info
            .conn_map
            .iter()
            .map(|x| (*x.key(), x.value().1.get()))
            // do not sync conn info of peers that are not reachable from any peer.
            .filter(|p| all_dst_peer_ids.contains(&p.0) || self.route_table.peer_reachable(p.0))
            .collect::<Vec<_>>();

        let mut conn_bitmap = RouteConnBitmap::new();
        conn_bitmap.bitmap = vec![0; (all_peer_ids.len() * all_peer_ids.len() + 7) / 8];
        conn_bitmap.peer_ids = all_peer_ids;

        let all_peer_ids = &conn_bitmap.peer_ids;
        for (peer_idx, (peer_id, _)) in all_peer_ids.iter().enumerate() {
            let connected = self.synced_route_info.conn_map.get(peer_id).unwrap();

            for (idx, (other_peer_id, _)) in all_peer_ids.iter().enumerate() {
                if connected.0.contains(other_peer_id) {
                    let bit_idx = peer_idx * all_peer_ids.len() + idx;
                    conn_bitmap.bitmap[bit_idx / 8] |= 1 << (bit_idx % 8);
                }
            }
        }

        *self.cached_local_conn_map.lock().unwrap() = conn_bitmap;
    }

    fn build_route_info(&self, session: &SyncRouteSession) -> Option<Vec<RoutePeerInfo>> {
        let mut route_infos = Vec::new();
        for item in self.synced_route_info.peer_infos.iter() {
            if session
                .check_saved_peer_info_update_to_date(item.value().peer_id, item.value().version)
            {
                continue;
            }

            // do not send unreachable peer info to dst peer.
            if !self.route_table.peer_reachable(*item.key()) {
                continue;
            }

            route_infos.push(item.value().clone());
        }

        if route_infos.is_empty() {
            None
        } else {
            Some(route_infos)
        }
    }

    fn build_conn_bitmap(&self, session: &SyncRouteSession) -> Option<RouteConnBitmap> {
        let mut need_update = false;
        for (peer_id, local_version) in self.cached_local_conn_map.lock().unwrap().peer_ids.iter() {
            let peer_version = session
                .dst_saved_conn_bitmap_version
                .get(&peer_id)
                .map(|item| item.get());
            if Some(*local_version) != peer_version {
                need_update = true;
                break;
            }
        }

        if !need_update {
            return None;
        }

        Some(self.cached_local_conn_map.lock().unwrap().clone())
    }

    async fn update_my_infos(&self) -> bool {
        let mut ret = self.update_my_peer_info();
        ret |= self.update_my_conn_info().await;
        ret
    }

    fn build_sync_request(
        &self,
        session: &SyncRouteSession,
    ) -> (Option<Vec<RoutePeerInfo>>, Option<RouteConnBitmap>) {
        let route_infos = self.build_route_info(&session);
        let conn_bitmap = self.build_conn_bitmap(&session);

        (route_infos, conn_bitmap)
    }

    fn clear_expired_peer(&self) {
        let now = SystemTime::now();
        let mut to_remove = Vec::new();
        for item in self.synced_route_info.peer_infos.iter() {
            if let Ok(d) = now.duration_since(item.value().last_update) {
                if d > REMOVE_DEAD_PEER_INFO_AFTER {
                    to_remove.push(*item.key());
                }
            }
        }

        for p in to_remove.iter() {
            self.synced_route_info.remove_peer(*p);
        }
    }

    async fn sync_route_with_peer(
        &self,
        dst_peer_id: PeerId,
        peer_rpc: Arc<PeerRpcManager>,
    ) -> bool {
        let Some(session) = self.get_session(dst_peer_id) else {
            // if session not exist, exit the sync loop.
            return true;
        };

        let my_peer_id = self.my_peer_id;

        let (peer_infos, conn_bitmap) = self.build_sync_request(&session);
        tracing::info!("my_id {:?}, pper_id: {:?}, peer_infos: {:?}, conn_bitmap: {:?}, synced_route_info: {:?} session: {:?}",
                       my_peer_id, dst_peer_id, peer_infos, conn_bitmap, self.synced_route_info, session);

        if peer_infos.is_none()
            && conn_bitmap.is_none()
            && !session.need_sync_initiator_info.load(Ordering::Relaxed)
        {
            return true;
        }

        session
            .need_sync_initiator_info
            .store(false, Ordering::Relaxed);

        let ret = peer_rpc
            .do_client_rpc_scoped(SERVICE_ID, dst_peer_id, |c| async {
                let client = RouteServiceClient::new(tarpc::client::Config::default(), c).spawn();
                let mut rpc_ctx = tarpc::context::current();
                rpc_ctx.deadline = SystemTime::now() + Duration::from_secs(3);
                client
                    .sync_route_info(
                        rpc_ctx,
                        my_peer_id,
                        session.my_session_id.load(Ordering::Relaxed),
                        session.we_are_initiator.load(Ordering::Relaxed),
                        peer_infos.clone(),
                        conn_bitmap.clone(),
                    )
                    .await
            })
            .await;

        match ret {
            Ok(Ok(ret)) => {
                session.rpc_tx_count.fetch_add(1, Ordering::Relaxed);

                session
                    .dst_is_initiator
                    .store(ret.is_initiator, Ordering::Relaxed);

                session.update_dst_session_id(ret.session_id);

                if let Some(peer_infos) = &peer_infos {
                    session.update_dst_saved_peer_info_version(&peer_infos);
                }

                if let Some(conn_bitmap) = &conn_bitmap {
                    session.update_dst_saved_conn_bitmap_version(&conn_bitmap);
                }
            }

            Ok(Err(Error::DuplicatePeerId)) => {
                panic!("duplicate peer id");
            }

            _ => {
                tracing::error!(?ret, ?my_peer_id, ?dst_peer_id, "sync_route_info failed");
                session
                    .need_sync_initiator_info
                    .store(true, Ordering::Relaxed);
            }
        }
        return false;
    }
}

#[derive(Clone)]
struct RouteSessionManager {
    service_impl: Weak<PeerRouteServiceImpl>,
    peer_rpc: Weak<PeerRpcManager>,

    sync_now_broadcast: tokio::sync::broadcast::Sender<()>,
}

impl Debug for RouteSessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteSessionManager")
            .field("dump_sessions", &self.dump_sessions())
            .finish()
    }
}

#[tarpc::server]
impl RouteService for RouteSessionManager {
    async fn sync_route_info(
        self,
        _: tarpc::context::Context,
        from_peer_id: PeerId,
        from_session_id: SessionId,
        is_initiator: bool,
        peer_infos: Option<Vec<RoutePeerInfo>>,
        conn_bitmap: Option<RouteConnBitmap>,
    ) -> Result<SyncRouteInfoResponse, Error> {
        let Some(service_impl) = self.service_impl.upgrade() else {
            return Err(Error::Stopped);
        };

        let my_peer_id = service_impl.my_peer_id;
        let session = self.get_or_start_session(from_peer_id)?;

        session.rpc_rx_count.fetch_add(1, Ordering::Relaxed);

        session.update_dst_session_id(from_session_id);

        if let Some(peer_infos) = &peer_infos {
            service_impl.synced_route_info.update_peer_infos(
                my_peer_id,
                from_peer_id,
                peer_infos,
            )?;
            session.update_dst_saved_peer_info_version(peer_infos);
        }

        if let Some(conn_bitmap) = &conn_bitmap {
            service_impl.synced_route_info.update_conn_map(&conn_bitmap);
            session.update_dst_saved_conn_bitmap_version(conn_bitmap);
        }

        service_impl.update_route_table_and_cached_local_conn_bitmap();

        tracing::info!(
            "sync_route_info: from_peer_id: {:?}, is_initiator: {:?}, peer_infos: {:?}, conn_bitmap: {:?}, synced_route_info: {:?} session: {:?}, new_route_table: {:?}",
            from_peer_id, is_initiator, peer_infos, conn_bitmap, service_impl.synced_route_info, session, service_impl.route_table);

        session
            .dst_is_initiator
            .store(is_initiator, Ordering::Relaxed);
        let is_initiator = session.we_are_initiator.load(Ordering::Relaxed);
        let session_id = session.my_session_id.load(Ordering::Relaxed);

        self.sync_now("sync_route_info");

        Ok(SyncRouteInfoResponse {
            is_initiator,
            session_id,
        })
    }
}

impl RouteSessionManager {
    fn new(service_impl: Arc<PeerRouteServiceImpl>, peer_rpc: Arc<PeerRpcManager>) -> Self {
        RouteSessionManager {
            service_impl: Arc::downgrade(&service_impl),
            peer_rpc: Arc::downgrade(&peer_rpc),

            sync_now_broadcast: tokio::sync::broadcast::channel(100).0,
        }
    }

    async fn session_task(
        peer_rpc: Weak<PeerRpcManager>,
        service_impl: Weak<PeerRouteServiceImpl>,
        dst_peer_id: PeerId,
        mut sync_now: tokio::sync::broadcast::Receiver<()>,
    ) {
        loop {
            let Some(service_impl) = service_impl.upgrade() else {
                return;
            };

            let Some(peer_rpc) = peer_rpc.upgrade() else {
                return;
            };

            while !service_impl
                .sync_route_with_peer(dst_peer_id, peer_rpc.clone())
                .await
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
                service_impl.update_my_infos().await;
            }
            sync_now.resubscribe();

            drop(service_impl);
            drop(peer_rpc);

            select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = sync_now.recv() => {}
            }
        }
    }

    fn stop_session(&self, peer_id: PeerId) -> Result<(), Error> {
        tracing::warn!(?peer_id, "stop ospf sync session");
        let Some(service_impl) = self.service_impl.upgrade() else {
            return Err(Error::Stopped);
        };
        service_impl.remove_session(peer_id);
        Ok(())
    }

    fn start_session_task(&self, session: &Arc<SyncRouteSession>) {
        if !session.task.is_running() {
            session.task.set_task(tokio::spawn(Self::session_task(
                self.peer_rpc.clone(),
                self.service_impl.clone(),
                session.dst_peer_id,
                self.sync_now_broadcast.subscribe(),
            )));
        }
    }

    fn get_or_start_session(&self, peer_id: PeerId) -> Result<Arc<SyncRouteSession>, Error> {
        let Some(service_impl) = self.service_impl.upgrade() else {
            return Err(Error::Stopped);
        };

        tracing::info!(?service_impl.my_peer_id, ?peer_id, "start ospf sync session");

        let session = service_impl.get_or_create_session(peer_id);
        self.start_session_task(&session);
        Ok(session)
    }

    #[tracing::instrument(skip(self))]
    async fn maintain_sessions(&self, service_impl: Arc<PeerRouteServiceImpl>) -> bool {
        let mut cur_dst_peer_id_to_initiate = None;
        let mut next_sleep_ms = 0;
        loop {
            let mut recv = self.sync_now_broadcast.subscribe();
            select! {
                _ = tokio::time::sleep(Duration::from_millis(next_sleep_ms)) => {}
                _ = recv.recv() => {}
            }

            let mut peers = service_impl.list_peers_from_interface::<Vec<_>>().await;
            peers.sort();

            let session_peers = self.list_session_peers();
            for peer_id in session_peers.iter() {
                if !peers.contains(peer_id) {
                    if Some(*peer_id) == cur_dst_peer_id_to_initiate {
                        cur_dst_peer_id_to_initiate = None;
                    }
                    let _ = self.stop_session(*peer_id);
                }
            }

            // find peer_ids that are not initiators.
            let initiator_candidates = peers
                .iter()
                .filter(|x| {
                    let Some(session) = service_impl.get_session(**x) else {
                        return true;
                    };
                    !session.dst_is_initiator.load(Ordering::Relaxed)
                })
                .map(|x| *x)
                .collect::<Vec<_>>();

            tracing::debug!(?service_impl.my_peer_id, ?peers, ?session_peers, ?initiator_candidates, "maintain_sessions begin");

            if initiator_candidates.is_empty() {
                next_sleep_ms = 1000;
                continue;
            }

            let mut new_initiator_dst = None;
            // if any peer has NoPAT or OpenInternet stun type, we should use it.
            for peer_id in initiator_candidates.iter() {
                let Some(nat_type) = service_impl.route_table.get_nat_type(*peer_id) else {
                    continue;
                };
                if nat_type == NatType::NoPat || nat_type == NatType::OpenInternet {
                    new_initiator_dst = Some(*peer_id);
                    break;
                }
            }
            if new_initiator_dst.is_none() {
                new_initiator_dst = Some(*initiator_candidates.first().unwrap());
            }

            if new_initiator_dst != cur_dst_peer_id_to_initiate {
                tracing::warn!(
                    "new_initiator: {:?}, prev: {:?}, my_id: {:?}",
                    new_initiator_dst,
                    cur_dst_peer_id_to_initiate,
                    service_impl.my_peer_id
                );
                // update initiator flag for previous session
                if let Some(cur_peer_id_to_initiate) = cur_dst_peer_id_to_initiate {
                    if let Some(session) = service_impl.get_session(cur_peer_id_to_initiate) {
                        session.update_initiator_flag(false);
                    }
                }

                cur_dst_peer_id_to_initiate = new_initiator_dst;
                // update initiator flag for new session
                let Ok(session) = self.get_or_start_session(new_initiator_dst.unwrap()) else {
                    tracing::warn!("get_or_start_session failed");
                    continue;
                };
                session.update_initiator_flag(true);
            }

            // clear sessions that are neither dst_initiator or we_are_initiator.
            for peer_id in session_peers.iter() {
                if let Some(session) = service_impl.get_session(*peer_id) {
                    if (session.dst_is_initiator.load(Ordering::Relaxed)
                        || session.we_are_initiator.load(Ordering::Relaxed)
                        || session.need_sync_initiator_info.load(Ordering::Relaxed))
                        && session.task.is_running()
                    {
                        continue;
                    }
                    let _ = self.stop_session(*peer_id);
                    assert_ne!(Some(*peer_id), cur_dst_peer_id_to_initiate);
                }
            }

            next_sleep_ms = 1000;
        }
    }

    fn list_session_peers(&self) -> Vec<PeerId> {
        let Some(service_impl) = self.service_impl.upgrade() else {
            return vec![];
        };

        service_impl.list_session_peers()
    }

    fn dump_sessions(&self) -> Result<String, Error> {
        let Some(service_impl) = self.service_impl.upgrade() else {
            return Err(Error::Stopped);
        };

        let mut ret = format!("my_peer_id: {:?}\n", service_impl.my_peer_id);
        for item in service_impl.sessions.iter() {
            ret += format!(
                "    session: {}, {}\n",
                item.key(),
                item.value().short_debug_string()
            )
            .as_str();
        }

        Ok(ret.to_string())
    }

    fn sync_now(&self, reason: &str) {
        let ret = self.sync_now_broadcast.send(());
        tracing::debug!(?ret, ?reason, "sync_now_broadcast.send");
    }
}

pub struct PeerRoute {
    my_peer_id: PeerId,
    global_ctx: ArcGlobalCtx,
    peer_rpc: Arc<PeerRpcManager>,

    service_impl: Arc<PeerRouteServiceImpl>,
    session_mgr: RouteSessionManager,

    tasks: std::sync::Mutex<JoinSet<()>>,
}

impl Debug for PeerRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerRoute")
            .field("my_peer_id", &self.my_peer_id)
            .field("service_impl", &self.service_impl)
            .field("session_mgr", &self.session_mgr)
            .finish()
    }
}

impl PeerRoute {
    pub fn new(
        my_peer_id: PeerId,
        global_ctx: ArcGlobalCtx,
        peer_rpc: Arc<PeerRpcManager>,
    ) -> Arc<Self> {
        let service_impl = Arc::new(PeerRouteServiceImpl::new(my_peer_id, global_ctx.clone()));
        let session_mgr = RouteSessionManager::new(service_impl.clone(), peer_rpc.clone());

        Arc::new(PeerRoute {
            my_peer_id,
            global_ctx: global_ctx.clone(),
            peer_rpc,

            service_impl,
            session_mgr,

            tasks: std::sync::Mutex::new(JoinSet::new()),
        })
    }

    async fn clear_expired_peer(service_impl: Arc<PeerRouteServiceImpl>) {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            service_impl.clear_expired_peer();
            // TODO: use debug log level for this.
            tracing::info!(?service_impl, "clear_expired_peer");
        }
    }

    #[tracing::instrument(skip(session_mgr))]
    async fn maintain_session_tasks(
        session_mgr: RouteSessionManager,
        service_impl: Arc<PeerRouteServiceImpl>,
    ) {
        session_mgr.maintain_sessions(service_impl).await;
    }

    #[tracing::instrument(skip(session_mgr))]
    async fn update_my_peer_info_routine(
        service_impl: Arc<PeerRouteServiceImpl>,
        session_mgr: RouteSessionManager,
    ) {
        let mut global_event_receiver = service_impl.global_ctx.subscribe();
        loop {
            if service_impl.update_my_infos().await {
                session_mgr.sync_now("update_my_infos");
            }

            if service_impl.cost_calculator_need_update() {
                tracing::debug!("cost_calculator_need_update");
                service_impl.update_route_table();
            }

            select! {
                ev = global_event_receiver.recv() => {
                    tracing::info!(?ev, "global event received in update_my_peer_info_routine");
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    async fn start(&self) {
        self.peer_rpc
            .run_service(SERVICE_ID, RouteService::serve(self.session_mgr.clone()));

        self.tasks
            .lock()
            .unwrap()
            .spawn(Self::update_my_peer_info_routine(
                self.service_impl.clone(),
                self.session_mgr.clone(),
            ));

        self.tasks
            .lock()
            .unwrap()
            .spawn(Self::maintain_session_tasks(
                self.session_mgr.clone(),
                self.service_impl.clone(),
            ));

        self.tasks
            .lock()
            .unwrap()
            .spawn(Self::clear_expired_peer(self.service_impl.clone()));
    }
}

#[async_trait::async_trait]
impl Route for PeerRoute {
    async fn open(&self, interface: RouteInterfaceBox) -> Result<u8, ()> {
        *self.service_impl.interface.lock().await = Some(interface);
        self.start().await;
        Ok(1)
    }

    async fn close(&self) {}

    async fn get_next_hop(&self, dst_peer_id: PeerId) -> Option<PeerId> {
        let route_table = &self.service_impl.route_table;
        route_table.get_next_hop(dst_peer_id).map(|x| x.0)
    }

    async fn get_next_hop_with_policy(
        &self,
        dst_peer_id: PeerId,
        policy: NextHopPolicy,
    ) -> Option<PeerId> {
        let route_table = if matches!(policy, NextHopPolicy::LeastCost) {
            &self.service_impl.route_table_with_cost
        } else {
            &self.service_impl.route_table
        };
        route_table.get_next_hop(dst_peer_id).map(|x| x.0)
    }

    async fn list_routes(&self) -> Vec<crate::rpc::Route> {
        let route_table = &self.service_impl.route_table;
        let mut routes = Vec::new();
        for item in route_table.peer_infos.iter() {
            if *item.key() == self.my_peer_id {
                continue;
            }
            let Some(next_hop_peer) = route_table.get_next_hop(*item.key()) else {
                continue;
            };
            let mut route: crate::rpc::Route = item.value().clone().into();
            route.next_hop_peer_id = next_hop_peer.0;
            route.cost = next_hop_peer.1;
            routes.push(route);
        }
        routes
    }

    async fn get_peer_id_by_ipv4(&self, ipv4_addr: &Ipv4Addr) -> Option<PeerId> {
        let route_table = &self.service_impl.route_table;
        if let Some(peer_id) = route_table.ipv4_peer_id_map.get(ipv4_addr) {
            return Some(*peer_id);
        }

        if let Some(peer_id) = route_table.get_peer_id_for_proxy(ipv4_addr) {
            return Some(peer_id);
        }

        tracing::debug!(?ipv4_addr, "no peer id for ipv4");
        None
    }

    async fn set_route_cost_fn(&self, _cost_fn: RouteCostCalculator) {
        *self.service_impl.cost_calculator.lock().unwrap() = Some(_cost_fn);
        self.service_impl.update_route_table();
    }

    async fn dump(&self) -> String {
        format!("{:#?}", self)
    }
}

impl PeerPacketFilter for Arc<PeerRoute> {}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::{atomic::Ordering, Arc},
        time::Duration,
    };

    use crate::{
        common::{global_ctx::tests::get_mock_global_ctx, PeerId},
        connector::udp_hole_punch::tests::replace_stun_info_collector,
        peers::{
            peer_manager::{PeerManager, RouteAlgoType},
            route_trait::{NextHopPolicy, Route, RouteCostCalculatorInterface},
            tests::connect_peer_manager,
        },
        rpc::NatType,
        tunnel::common::tests::wait_for_condition,
    };

    use super::PeerRoute;

    async fn create_mock_route(peer_mgr: Arc<PeerManager>) -> Arc<PeerRoute> {
        let peer_route = PeerRoute::new(
            peer_mgr.my_peer_id(),
            peer_mgr.get_global_ctx(),
            peer_mgr.get_peer_rpc_mgr(),
        );
        peer_mgr.add_route(peer_route.clone()).await;
        peer_route
    }

    fn get_rpc_counter(route: &Arc<PeerRoute>, peer_id: PeerId) -> (u32, u32) {
        let session = route.service_impl.get_session(peer_id).unwrap();
        (
            session.rpc_tx_count.load(Ordering::Relaxed),
            session.rpc_rx_count.load(Ordering::Relaxed),
        )
    }

    fn get_is_initiator(route: &Arc<PeerRoute>, peer_id: PeerId) -> (bool, bool) {
        let session = route.service_impl.get_session(peer_id).unwrap();
        (
            session.we_are_initiator.load(Ordering::Relaxed),
            session.dst_is_initiator.load(Ordering::Relaxed),
        )
    }

    async fn create_mock_pmgr() -> Arc<PeerManager> {
        let (s, _r) = tokio::sync::mpsc::channel(1000);
        let peer_mgr = Arc::new(PeerManager::new(
            RouteAlgoType::None,
            get_mock_global_ctx(),
            s,
        ));
        replace_stun_info_collector(peer_mgr.clone(), NatType::Unknown);
        peer_mgr.run().await.unwrap();
        peer_mgr
    }

    fn check_rpc_counter(route: &Arc<PeerRoute>, peer_id: PeerId, max_tx: u32, max_rx: u32) {
        let (tx1, rx1) = get_rpc_counter(route, peer_id);
        assert!(tx1 <= max_tx);
        assert!(rx1 <= max_rx);
    }

    #[tokio::test]
    async fn ospf_route_2node() {
        let p_a = create_mock_pmgr().await;
        let p_b = create_mock_pmgr().await;
        connect_peer_manager(p_a.clone(), p_b.clone()).await;

        let r_a = create_mock_route(p_a.clone()).await;
        let r_b = create_mock_route(p_b.clone()).await;

        for r in vec![r_a.clone(), r_b.clone()].iter() {
            wait_for_condition(
                || async { r.list_routes().await.len() == 1 },
                Duration::from_secs(5),
            )
            .await;
        }

        tokio::time::sleep(Duration::from_secs(3)).await;

        assert_eq!(2, r_a.service_impl.synced_route_info.peer_infos.len());
        assert_eq!(2, r_b.service_impl.synced_route_info.peer_infos.len());

        for s in r_a.service_impl.sessions.iter() {
            assert!(s.value().task.is_running());
        }

        assert_eq!(
            r_a.service_impl
                .synced_route_info
                .peer_infos
                .get(&p_a.my_peer_id())
                .unwrap()
                .version,
            r_a.service_impl
                .get_session(p_b.my_peer_id())
                .unwrap()
                .dst_saved_peer_info_versions
                .get(&p_a.my_peer_id())
                .unwrap()
                .value()
                .0
                .load(Ordering::Relaxed)
        );

        assert_eq!((1, 1), get_rpc_counter(&r_a, p_b.my_peer_id()));
        assert_eq!((1, 1), get_rpc_counter(&r_b, p_a.my_peer_id()));

        let i_a = get_is_initiator(&r_a, p_b.my_peer_id());
        let i_b = get_is_initiator(&r_b, p_a.my_peer_id());
        assert_eq!(i_a.0, i_b.1);
        assert_eq!(i_b.0, i_a.1);

        drop(r_b);
        drop(p_b);

        wait_for_condition(
            || async { r_a.list_routes().await.len() == 0 },
            Duration::from_secs(5),
        )
        .await;

        wait_for_condition(
            || async { r_a.service_impl.sessions.is_empty() },
            Duration::from_secs(5),
        )
        .await;
    }

    #[tokio::test]
    async fn ospf_route_multi_node() {
        let p_a = create_mock_pmgr().await;
        let p_b = create_mock_pmgr().await;
        let p_c = create_mock_pmgr().await;
        connect_peer_manager(p_a.clone(), p_b.clone()).await;
        connect_peer_manager(p_c.clone(), p_b.clone()).await;

        let r_a = create_mock_route(p_a.clone()).await;
        let r_b = create_mock_route(p_b.clone()).await;
        let r_c = create_mock_route(p_c.clone()).await;

        for r in vec![r_a.clone(), r_b.clone(), r_c.clone()].iter() {
            wait_for_condition(
                || async { r.service_impl.synced_route_info.peer_infos.len() == 3 },
                Duration::from_secs(5),
            )
            .await;
        }

        connect_peer_manager(p_a.clone(), p_c.clone()).await;
        // for full-connected 3 nodes, the sessions between them may be a cycle or a line
        wait_for_condition(
            || async {
                let mut lens = vec![
                    r_a.service_impl.sessions.len(),
                    r_b.service_impl.sessions.len(),
                    r_c.service_impl.sessions.len(),
                ];
                lens.sort();

                lens == vec![1, 1, 2] || lens == vec![2, 2, 2]
            },
            Duration::from_secs(3),
        )
        .await;

        let p_d = create_mock_pmgr().await;
        let r_d = create_mock_route(p_d.clone()).await;
        connect_peer_manager(p_d.clone(), p_a.clone()).await;
        connect_peer_manager(p_d.clone(), p_b.clone()).await;
        connect_peer_manager(p_d.clone(), p_c.clone()).await;

        // find the smallest peer_id, which should be a center node
        let mut all_route = vec![r_a.clone(), r_b.clone(), r_c.clone(), r_d.clone()];
        all_route.sort_by(|a, b| a.my_peer_id.cmp(&b.my_peer_id));
        let mut all_peer_mgr = vec![p_a.clone(), p_b.clone(), p_c.clone(), p_d.clone()];
        all_peer_mgr.sort_by(|a, b| a.my_peer_id().cmp(&b.my_peer_id()));

        wait_for_condition(
            || async { all_route[0].service_impl.sessions.len() == 3 },
            Duration::from_secs(3),
        )
        .await;

        for r in all_route.iter() {
            println!("session: {}", r.session_mgr.dump_sessions().unwrap());
        }

        let p_e = create_mock_pmgr().await;
        let r_e = create_mock_route(p_e.clone()).await;
        let last_p = all_peer_mgr.last().unwrap();
        connect_peer_manager(p_e.clone(), last_p.clone()).await;

        wait_for_condition(
            || async { r_e.session_mgr.list_session_peers().len() == 1 },
            Duration::from_secs(3),
        )
        .await;

        for s in r_e.service_impl.sessions.iter() {
            assert!(s.value().task.is_running());
        }

        tokio::time::sleep(Duration::from_secs(2)).await;

        check_rpc_counter(&r_e, last_p.my_peer_id(), 2, 2);

        for r in all_route.iter() {
            if r.my_peer_id != last_p.my_peer_id() {
                wait_for_condition(
                    || async {
                        r.get_next_hop(p_e.my_peer_id()).await == Some(last_p.my_peer_id())
                    },
                    Duration::from_secs(3),
                )
                .await;
            } else {
                wait_for_condition(
                    || async { r.get_next_hop(p_e.my_peer_id()).await == Some(p_e.my_peer_id()) },
                    Duration::from_secs(3),
                )
                .await;
            }
        }
    }

    async fn check_route_sanity(p: &Arc<PeerRoute>, routable_peers: Vec<Arc<PeerManager>>) {
        let synced_info = &p.service_impl.synced_route_info;
        for routable_peer in routable_peers.iter() {
            // check conn map
            let conns = synced_info
                .conn_map
                .get(&routable_peer.my_peer_id())
                .unwrap();

            assert_eq!(
                conns.0,
                routable_peer
                    .get_peer_map()
                    .list_peers()
                    .await
                    .into_iter()
                    .collect::<BTreeSet<PeerId>>()
            );

            // check peer infos
            let peer_info = synced_info
                .peer_infos
                .get(&routable_peer.my_peer_id())
                .unwrap();
            assert_eq!(peer_info.peer_id, routable_peer.my_peer_id());
        }
    }

    async fn print_routes(peers: Vec<Arc<PeerRoute>>) {
        for p in peers.iter() {
            println!("p:{:?}, route: {:#?}", p.my_peer_id, p.list_routes().await);
        }
    }

    #[tokio::test]
    async fn ospf_route_3node_disconnect() {
        let p_a = create_mock_pmgr().await;
        let p_b = create_mock_pmgr().await;
        let p_c = create_mock_pmgr().await;
        connect_peer_manager(p_a.clone(), p_b.clone()).await;
        connect_peer_manager(p_c.clone(), p_b.clone()).await;

        let mgrs = vec![p_a.clone(), p_b.clone(), p_c.clone()];

        let r_a = create_mock_route(p_a.clone()).await;
        let r_b = create_mock_route(p_b.clone()).await;
        let r_c = create_mock_route(p_c.clone()).await;

        for r in vec![r_a.clone(), r_b.clone(), r_c.clone()].iter() {
            wait_for_condition(
                || async { r.service_impl.synced_route_info.peer_infos.len() == 3 },
                Duration::from_secs(5),
            )
            .await;
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        print_routes(vec![r_a.clone(), r_b.clone(), r_c.clone()]).await;
        check_route_sanity(&r_a, mgrs.clone()).await;
        check_route_sanity(&r_b, mgrs.clone()).await;
        check_route_sanity(&r_c, mgrs.clone()).await;

        assert_eq!(2, r_a.list_routes().await.len());

        drop(mgrs);
        drop(r_c);
        drop(p_c);

        for r in vec![r_a.clone(), r_b.clone()].iter() {
            wait_for_condition(
                || async { r.list_routes().await.len() == 1 },
                Duration::from_secs(5),
            )
            .await;
        }
    }

    #[tokio::test]
    async fn peer_reconnect() {
        let p_a = create_mock_pmgr().await;
        let p_b = create_mock_pmgr().await;
        let r_a = create_mock_route(p_a.clone()).await;
        let r_b = create_mock_route(p_b.clone()).await;

        connect_peer_manager(p_a.clone(), p_b.clone()).await;

        wait_for_condition(
            || async { r_a.list_routes().await.len() == 1 },
            Duration::from_secs(5),
        )
        .await;

        assert_eq!(1, r_b.list_routes().await.len());

        check_rpc_counter(&r_a, p_b.my_peer_id(), 2, 2);

        p_a.get_peer_map()
            .close_peer(p_b.my_peer_id())
            .await
            .unwrap();
        wait_for_condition(
            || async { r_a.list_routes().await.len() == 0 },
            Duration::from_secs(5),
        )
        .await;

        // reconnect
        connect_peer_manager(p_a.clone(), p_b.clone()).await;
        wait_for_condition(
            || async { r_a.list_routes().await.len() == 1 },
            Duration::from_secs(5),
        )
        .await;

        // wait session init
        tokio::time::sleep(Duration::from_secs(1)).await;

        println!("session: {:?}", r_a.session_mgr.dump_sessions());
        check_rpc_counter(&r_a, p_b.my_peer_id(), 2, 2);
    }

    #[tokio::test]
    async fn test_cost_calculator() {
        let p_a = create_mock_pmgr().await;
        let p_b = create_mock_pmgr().await;
        let p_c = create_mock_pmgr().await;
        let p_d = create_mock_pmgr().await;
        connect_peer_manager(p_a.clone(), p_b.clone()).await;
        connect_peer_manager(p_a.clone(), p_c.clone()).await;
        connect_peer_manager(p_d.clone(), p_b.clone()).await;
        connect_peer_manager(p_d.clone(), p_c.clone()).await;
        connect_peer_manager(p_b.clone(), p_c.clone()).await;

        let _r_a = create_mock_route(p_a.clone()).await;
        let _r_b = create_mock_route(p_b.clone()).await;
        let _r_c = create_mock_route(p_c.clone()).await;
        let r_d = create_mock_route(p_d.clone()).await;

        // in normal mode, packet from p_c should directly forward to p_a
        wait_for_condition(
            || async { r_d.get_next_hop(p_a.my_peer_id()).await != None },
            Duration::from_secs(5),
        )
        .await;

        struct TestCostCalculator {
            p_a_peer_id: PeerId,
            p_b_peer_id: PeerId,
            p_c_peer_id: PeerId,
            p_d_peer_id: PeerId,
        }

        impl RouteCostCalculatorInterface for TestCostCalculator {
            fn calculate_cost(&self, src: PeerId, dst: PeerId) -> i32 {
                if src == self.p_d_peer_id && dst == self.p_b_peer_id {
                    return 100;
                }

                if src == self.p_d_peer_id && dst == self.p_c_peer_id {
                    return 1;
                }

                if src == self.p_c_peer_id && dst == self.p_a_peer_id {
                    return 101;
                }

                if src == self.p_b_peer_id && dst == self.p_a_peer_id {
                    return 1;
                }

                if src == self.p_c_peer_id && dst == self.p_b_peer_id {
                    return 2;
                }

                1
            }
        }

        r_d.set_route_cost_fn(Box::new(TestCostCalculator {
            p_a_peer_id: p_a.my_peer_id(),
            p_b_peer_id: p_b.my_peer_id(),
            p_c_peer_id: p_c.my_peer_id(),
            p_d_peer_id: p_d.my_peer_id(),
        }))
        .await;

        // after set cost, packet from p_c should forward to p_b first
        wait_for_condition(
            || async {
                r_d.get_next_hop_with_policy(p_a.my_peer_id(), NextHopPolicy::LeastCost)
                    .await
                    == Some(p_c.my_peer_id())
            },
            Duration::from_secs(5),
        )
        .await;

        wait_for_condition(
            || async {
                r_d.get_next_hop_with_policy(p_a.my_peer_id(), NextHopPolicy::LeastHop)
                    .await
                    == Some(p_b.my_peer_id())
            },
            Duration::from_secs(5),
        )
        .await;
    }
}
