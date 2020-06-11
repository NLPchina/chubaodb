// Copyright 2020 The Chubao Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or
// implied. See the License for the specific language governing
// permissions and limitations under the License.
use crate::client::meta_client::MetaClient;
use crate::pserver::raft::*;
use crate::pserver::simba::simba::Simba;
use crate::pserverpb::*;
use crate::util::{coding, config, entity::*, error::*};
use crate::*;
use async_std::{sync::channel, task};
use log::{error, info};
use raft4rs::{
    entity::{Decode, Entry},
    error::*,
    raft::Raft,
    server::Server as RaftServer,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering::SeqCst},
    Arc, Mutex, RwLock,
};

enum Store {
    Leader {
        partition: Arc<Partition>,
        raft: Arc<Raft>,
        simba: Arc<Simba>,
    },
    Member {
        partition: Arc<Partition>,
        raft: Arc<Raft>,
        simba: Arc<Simba>,
    },
}

impl Store {
    fn is_leader_type(&self) -> bool {
        match self {
            Self::Leader { .. } => true,
            _ => false,
        }
    }

    fn leader_simba(&self) -> ASResult<(Arc<Simba>, Arc<Raft>)> {
        match self {
            Self::Leader { simba, raft, .. } => Ok((simba.clone(), raft.clone())),
            _ => result!(Code::PartitionNotLeader, "simba partition not leader"),
        }
    }

    fn simba(&self) -> ASResult<Arc<Simba>> {
        match self {
            Self::Leader { simba, .. } | Self::Member { simba, .. } => Ok(simba.clone()),
        }
    }

    fn raft(&self) -> ASResult<Arc<Raft>> {
        match self {
            Self::Leader { raft, .. } | Self::Member { raft, .. } => Ok(raft.clone()),
        }
    }

    fn partition(&self) -> Arc<Partition> {
        match self {
            Self::Leader { partition, .. } | Self::Member { partition, .. } => partition.clone(),
        }
    }
}

pub struct PartitionService {
    pub server_id: AtomicU64,
    simba_map: RwLock<HashMap<(u32, u32), Arc<Store>>>,
    pub conf: Arc<config::Config>,
    pub lock: Mutex<usize>,
    meta_client: Arc<MetaClient>,
    raft_server: Option<RaftServer>,
}

impl PartitionService {
    pub fn new(conf: Arc<config::Config>) -> Arc<Self> {
        Arc::new(PartitionService {
            server_id: AtomicU64::new(0),
            simba_map: RwLock::new(HashMap::new()),
            conf: conf.clone(),
            lock: Mutex::new(0),
            meta_client: Arc::new(MetaClient::new(conf)),
            raft_server: None,
        })
    }

    pub async fn init(self: &mut Arc<Self>) -> ASResult<()> {
        let ps = match self
            .meta_client
            .register(self.conf.global.ip.as_str(), self.conf.ps.rpc_port as u32)
            .await
        {
            Ok(p) => {
                info!("register to master ok: node_id:{:?} ", p.id);
                p
            }
            Err(e) => {
                return result_def!("{}", e.to_string());
            }
        };

        match ps.id {
            Some(id) => self.server_id.store(id as u64, SeqCst),
            None => {
                return result_def!("got id for master has err got:{:?} ", ps.id);
            }
        }

        info!("register server line:{:?}", ps);

        let raft_server = RaftServer::new(
            make_raft_conf(self.server_id.load(SeqCst), &self.conf),
            NodeResolver::new(self.meta_client.clone()),
        );

        Arc::get_mut(self).unwrap().raft_server = Some(raft_server);

        for wp in ps.write_partitions {
            if let Err(e) = self
                .init_partition(wp.collection_id, wp.id, wp.replicas, false, wp.version)
                .await
            {
                error!("init partition has err:{}", e.to_string());
            };
        }

        Ok(())
    }

    pub async fn init_partition(
        self: &Arc<Self>,
        collection_id: u32,
        partition_id: u32,
        replicas: Vec<Replica>,
        _readonly: bool,
        version: u64,
    ) -> ASResult<()> {
        info!(
            "to load partition:{} partition:{} exisit:{}",
            collection_id,
            partition_id,
            self.simba_map
                .read()
                .unwrap()
                .contains_key(&(collection_id, partition_id))
        );

        let _ = self.lock.lock().unwrap();
        info!("Start init_partition");

        if self
            .simba_map
            .read()
            .unwrap()
            .get(&(collection_id, partition_id))
            .is_some()
        {
            return Ok(());
        }

        let collection = Arc::new(self.meta_client.get_collection_by_id(collection_id).await?);

        if version > 0 {
            self.check_partition_version(collection_id, partition_id, version)
                .await?;
        }

        let partition = Arc::new(Partition {
            id: partition_id,
            collection_id: collection_id,
            replicas: replicas,
            leader: format!("{}:{}", self.conf.global.ip, self.conf.ps.rpc_port), //TODO: first need set leader.
            version: version + 1,
        });

        let simba = Simba::new(self.conf.clone(), collection.clone(), partition.clone())?;

        let replicas: Vec<u64> = partition
            .replicas
            .iter()
            .map(|r| r.node_id as u64)
            .collect();

        let raft = conver(
            self.raft_server
                .as_ref()
                .unwrap()
                .create_raft(
                    coding::merge_u32(collection.id, partition.id),
                    0,
                    replicas[0],
                    &replicas,
                    NodeStateMachine::new(
                        Some(simba.clone()),
                        collection.clone(),
                        partition.clone(),
                        self.clone(),
                    ),
                )
                .await,
        )?;

        self.init_simba_by_raft(&simba, &raft).await?;

        self.simba_map.write().unwrap().insert(
            (collection_id, partition_id),
            Arc::new(Store::Member {
                simba: simba,
                partition: partition,
                raft: raft,
            }),
        );

        Ok(())
    }

    async fn check_partition_version(&self, cid: u32, pid: u32, version: u64) -> ASResult<()> {
        let partition = self.meta_client.get_partition(cid, pid).await?;

        if partition.version > version {
            return result!(
                Code::VersionErr,
                "the collection:{} partition:{} version not right expected:{} found:{}",
                cid,
                pid,
                version,
                partition.version
            );
        }
        Ok(())
    }

    //offload partition , if partition not exist , it will return success
    pub fn offload_partition(&self, req: PartitionRequest) -> ASResult<GeneralResponse> {
        info!(
            "to offload partition:{} partition:{} exisit:{}",
            req.collection_id,
            req.partition_id,
            self.simba_map
                .read()
                .unwrap()
                .contains_key(&(req.collection_id, req.partition_id))
        );
        if let Some(store) = self
            .simba_map
            .write()
            .unwrap()
            .remove(&(req.collection_id, req.partition_id))
        {
            store.simba()?.stop();
            crate::sleep!(300);
            while Arc::strong_count(&store) > 1 {
                info!(
                    "wait release store collection:{} partition:{} now is :{}",
                    req.collection_id,
                    req.partition_id,
                    Arc::strong_count(&store)
                );
                crate::sleep!(300);
            }
            store.simba()?.release();
        }
        make_general_success()
    }

    pub async fn apply_leader_change(
        &self,
        collection: &Arc<Collection>,
        partition: &Arc<Partition>,
        leader_id: u64,
    ) -> ASResult<()> {
        let (cid, pid) = (collection.id, partition.id);

        let store = match self.simba_map.read().unwrap().get(&(cid, pid)) {
            Some(store) => store.clone(),
            None => {
                return result_def!(
                    "not found partition_id:{} collection_id:{} in server",
                    cid,
                    pid
                );
            }
        };
        if self.server_id.load(SeqCst) == leader_id {
            if store.is_leader_type() {
                return Ok(());
            }

            let store = Store::Leader {
                partition: store.partition(),
                raft: store.raft()?,
                simba: store.simba()?,
            };

            self.simba_map
                .write()
                .unwrap()
                .insert((cid, pid), Arc::new(store));
        } else {
            if !store.is_leader_type() {
                return Ok(());
            }

            let store = if self.conf.global.shared_disk {
                panic!("not support ")
            } else {
                Store::Member {
                    partition: partition.clone(),
                    raft: store.raft()?,
                    simba: store.simba()?,
                }
            };

            self.simba_map
                .write()
                .unwrap()
                .insert((cid, pid), Arc::new(store));
        }

        self.take_heartbeat().await
    }

    async fn init_simba_by_raft(&self, simba: &Arc<Simba>, raft: &Arc<Raft>) -> RaftResult<()> {
        let index = simba.get_raft_index() + 1;
        let mut iter = raft.store.iter(index).await?;

        while let Some(body) = iter.next(&raft.store).await? {
            match Entry::decode(&body)? {
                Entry::Commit { index, commond, .. } => {
                    if let Err(e) = simba.do_write(index, &commond, true) {
                        error!("init raft log has err:{:?} line:{:?}", e, commond);
                    }
                }
                Entry::LeaderChange { .. } => {}
                Entry::MemberChange { .. } => {
                    //TODO: member change ........
                }
                _ => panic!("not support"),
            }
        }
        Ok(())
    }

    pub async fn take_heartbeat(&self) -> ASResult<()> {
        let _ = self.lock.lock().unwrap();

        let wps = self
            .simba_map
            .read()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.is_leader_type())
            .map(|(_, s)| Partition::clone(&*s.simba().unwrap().base.partition))
            .collect::<Vec<Partition>>();

        self.meta_client
            .put_pserver(&PServer {
                id: Some(self.server_id.load(SeqCst) as u32),
                addr: format!("{}:{}", self.conf.global.ip.as_str(), self.conf.ps.rpc_port),
                write_partitions: wps,
                zone: self.conf.ps.zone.clone(),
                modify_time: 0,
            })
            .await
    }

    pub async fn write(&self, req: WriteDocumentRequest) -> ASResult<GeneralResponse> {
        let (simba, raft) = if let Some(store) = self
            .simba_map
            .read()
            .unwrap()
            .get(&(req.collection_id, req.partition_id))
        {
            store.leader_simba()?.clone()
        } else {
            return Err(make_not_found_err(req.collection_id, req.partition_id)?);
        };

        match simba.write(req, raft).await {
            Ok(_) | Err(ASError::Success) => Ok(GeneralResponse {
                code: Code::Success as i32,
                message: String::from("success"),
            }),
            Err(ASError::Error(c, m)) => Ok(GeneralResponse {
                code: c as i32,
                message: m,
            }),
        }
    }

    pub fn get(&self, req: GetDocumentRequest) -> ASResult<DocumentResponse> {
        let store = if let Some(store) = self
            .simba_map
            .read()
            .unwrap()
            .get(&(req.collection_id, req.partition_id))
        {
            store.clone()
        } else {
            make_not_found_err(req.collection_id, req.partition_id)?
        };

        Ok(DocumentResponse {
            code: Code::Success as i32,
            message: String::from("success"),
            doc: store.simba()?.get(req.id.as_str(), req.sort_key.as_str())?,
        })
    }

    pub async fn count(&self, req: CountDocumentRequest) -> ASResult<CountDocumentResponse> {
        let mut cdr = CountDocumentResponse {
            code: Code::Success as i32,
            estimate_count: 0,
            index_count: 0,
            db_count: 0,
            vectors_count: Vec::new(),
            message: String::default(),
        };

        for collection_partition_id in req.cpids.iter() {
            let cpid = coding::split_u32(*collection_partition_id);
            let simba = if let Some(store) = self.simba_map.read().unwrap().get(&cpid) {
                store.simba()?.clone()
            } else {
                return make_not_found_err(cpid.0, cpid.1);
            };

            match simba.count() {
                Ok(v) => {
                    cdr.estimate_count += v.estimate_count;
                    cdr.index_count += v.index_count;
                    cdr.db_count += v.db_count;
                    for (i, ic) in v.vectors_count.into_iter().enumerate() {
                        match cdr.vectors_count.get_mut(i) {
                            Some(ic2) => ic2.count += ic.count,
                            None => cdr.vectors_count.push(ic),
                        }
                    }
                }
                Err(e) => {
                    cdr.code = e.code() as i32;
                    cdr.message.push_str(&format!(
                        "collection_partition_id:{} has err:{}",
                        collection_partition_id, e
                    ));
                }
            }
        }

        return Ok(cdr);
    }

    pub async fn search(&self, sdreq: SearchDocumentRequest) -> ASResult<SearchDocumentResponse> {
        assert_ne!(sdreq.cpids.len(), 0);

        let len = sdreq.cpids.len();

        let (tx, rx) = channel(len);

        let sdreq = Arc::new(sdreq);

        for cpid in sdreq.cpids.iter() {
            let cpid = coding::split_u32(*cpid);
            if let Some(store) = self.simba_map.read().unwrap().get(&cpid) {
                if let Ok(simba) = store.simba() {
                    let simba = simba.clone();
                    let tx = tx.clone();
                    let sdreq = sdreq.clone();
                    task::spawn(async move {
                        tx.send(simba.search(sdreq)).await;
                    });
                } else {
                    return make_not_found_err(cpid.0, cpid.1);
                }
            } else {
                return make_not_found_err(cpid.0, cpid.1);
            }
        }

        let mut dist = rx.recv().await?;
        for _ in 0..len - 1 {
            dist = merge_search_document_response(dist, rx.recv().await.unwrap());
        }
        dist.hits.sort_by(|v1, v2| {
            if v1.score >= v2.score {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        });

        if dist.hits.len() > sdreq.size as usize {
            unsafe {
                dist.hits.set_len(sdreq.size as usize);
            }
        }

        Ok(dist)
    }

    pub fn status(&self, _request: GeneralRequest) -> ASResult<GeneralResponse> {
        Ok(GeneralResponse {
            code: Code::Success as i32,
            message: String::from("ok"),
        })
    }
}

impl PartitionService {
    pub fn command(&self, command: CommandRequest) -> ASResult<Vec<u8>> {
        let value: Value = serde_json::from_slice(command.body.as_slice())?;

        match value["method"].as_str().unwrap() {
            "file_info" => self._file_info(value),
            _ => result_def!("not found method:{}", value["method"]),
        }
    }

    fn _file_info(&self, value: Value) -> ASResult<Vec<u8>> {
        let path = value["path"].as_str().unwrap().to_string();

        let mut result = Vec::new();

        for entry in std::fs::read_dir(path)? {
            let file = conver(entry)?;
            let meta = file.metadata()?;
            result.push(json!({
                "path": file.file_name().into_string(),
                "len":meta.len(),
                "modified": meta.modified().unwrap(),
            }));
        }

        conver(serde_json::to_vec(&result))
    }
}

fn make_not_found_err<T>(cid: u32, pid: u32) -> ASResult<T> {
    result!(
        Code::RocksDBNotFound,
        "not found collection:{}  partition by id:{}",
        cid,
        pid
    )
}

fn make_general_success() -> ASResult<GeneralResponse> {
    Ok(GeneralResponse {
        code: Code::Success as i32,
        message: String::from("success"),
    })
}
