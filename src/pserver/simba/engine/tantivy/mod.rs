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
pub mod bitmap_collector;

use crate::pserver::simba::engine::engine::{BaseEngine, Engine};
use crate::pserver::simba::engine::rocksdb::RocksDB;
use crate::pserverpb::*;
use crate::util::coding::iid_coding;
use crate::util::entity::Field::{float, int, string, text};
use crate::util::error::*;
use crate::*;
use log::{debug, error, info, warn};
use roaring::RoaringBitmap;
use std::fs;
use std::ops::Deref;
use std::path::Path;
use std::sync::{
    atomic::{AtomicU32, Ordering::SeqCst},
    mpsc::{channel, Receiver, Sender},
    Arc, Mutex, RwLock,
};
use std::time::SystemTime;
use tantivy::{
    collector::{Count, MultiCollector, TopDocs},
    directory::MmapDirectory,
    query::{QueryParser, TermQuery},
    schema,
    schema::{Field, FieldType as TantivyFT, FieldValue, IndexRecordOption, Schema, Value},
    Document, Index, IndexReader, IndexWriter, ReloadPolicy, Term,
};

const INDEXER_MEMORY_SIZE: usize = 1_000_000_000;
const INDEXER_THREAD: usize = 1;
const ID: &'static str = "_iid";
const ID_BYTES: &'static str = "_iid_bytes";
const ID_INDEX: u32 = 0;
const ID_BYTES_INDEX: u32 = 1;
const INDEX_DIR_NAME: &'static str = "index";

pub enum Event {
    Delete(u32),
    // Update(old_iid , new_iid)
    Update(u32, u32),
    Stop,
}

pub struct Tantivy {
    base: Arc<BaseEngine>,
    index: Index,
    index_writer: RwLock<IndexWriter>,
    index_reader: IndexReader,
    field_num: usize,
    db: Arc<RocksDB>,
    tx: Mutex<Sender<Event>>,
    status: AtomicU32,
}

impl Deref for Tantivy {
    type Target = Arc<BaseEngine>;
    fn deref<'a>(&'a self) -> &'a Arc<BaseEngine> {
        &self.base
    }
}

impl Tantivy {
    pub fn new(db: Arc<RocksDB>, base: Arc<BaseEngine>) -> ASResult<Arc<Tantivy>> {
        let now = SystemTime::now();

        let mut schema_builder = Schema::builder();
        schema_builder.add_i64_field(ID, schema::IntOptions::default().set_indexed());
        schema_builder.add_bytes_field(ID_BYTES); //if you want put default filed mut modify validate method - 2 in code

        for i in base.collection.scalar_field_index.iter() {
            let field = &base.collection.fields[*i as usize];

            match field {
                int(_f) => {
                    schema_builder
                        .add_i64_field(field.name(), schema::IntOptions::default().set_indexed());
                }
                float(_f) => {
                    schema_builder
                        .add_f64_field(field.name(), schema::IntOptions::default().set_indexed());
                }
                string(_f) => {
                    schema_builder.add_text_field(field.name(), schema::STRING);
                }
                text(_f) => {
                    schema_builder.add_text_field(field.name(), schema::TEXT);
                }
                _ => return result_def!("thie type:{:?} can not make index", field),
            }
        }

        let schema = schema_builder.build();
        let field_num = schema.fields().count();

        let index_dir = base.base_path().join(Path::new(INDEX_DIR_NAME));
        if !index_dir.exists() {
            fs::create_dir_all(&index_dir)?;
        }

        let index = conver(Index::open_or_create::<MmapDirectory>(
            MmapDirectory::open(index_dir.to_str().unwrap())?,
            schema,
        ))?;

        let index_writer = index
            .writer_with_num_threads(INDEXER_THREAD, INDEXER_MEMORY_SIZE)
            .unwrap();

        let index_reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommit)
            .try_into()
            .unwrap();

        let (tx, rx) = channel::<Event>();

        db.arc_count.fetch_add(1, SeqCst);
        let tantivy = Arc::new(Tantivy {
            base: base,
            index: index,
            index_writer: RwLock::new(index_writer),
            index_reader: index_reader,
            field_num: field_num,
            db: db,
            tx: Mutex::new(tx),
            status: AtomicU32::new(0),
        });

        Tantivy::start_job(tantivy.clone(), rx);

        info!(
            "init index by collection:{} partition:{} success , use time:{:?} ",
            tantivy.collection.id,
            tantivy.partition.id,
            SystemTime::now().duration_since(now).unwrap().as_millis(),
        );

        Ok(tantivy)
    }

    pub fn release(&self) {
        warn!("partition:{} index released", self.partition.id);
    }

    pub fn count(&self) -> ASResult<u64> {
        let searcher = self.index_reader.searcher();
        let mut sum = 0;
        for sr in searcher.segment_readers() {
            sum += sr.num_docs() as u64;
        }
        Ok(sum)
    }

    pub fn filter(
        &self,
        sdr: Arc<SearchDocumentRequest>,
    ) -> ASResult<(Option<RoaringBitmap>, u64)> {
        if sdr.query == "*" {
            return Ok((None, self.count()?));
        }

        self.check_index()?;
        let searcher = self.index_reader.searcher();
        let query_parser = QueryParser::for_index(
            &self.index,
            sdr.def_fields
                .iter()
                .map(|s| self.index.schema().get_field(s).unwrap())
                .collect(),
        );
        let q = conver(query_parser.parse_query(sdr.query.as_str()))?;
        let result = conver(searcher.search(&q, &bitmap_collector::Bitmap))?;
        let len = result.len();
        Ok((Some(result), len))
    }

    pub fn query(&self, sdr: Arc<SearchDocumentRequest>) -> ASResult<SearchDocumentResponse> {
        self.check_index()?;
        let searcher = self.index_reader.searcher();
        let query_parser = QueryParser::for_index(
            &self.index,
            sdr.def_fields
                .iter()
                .map(|s| self.index.schema().get_field(s).unwrap())
                .collect(),
        );
        let size = sdr.size as usize;
        let q = conver(query_parser.parse_query(sdr.query.as_str()))?;

        let mut collectors = MultiCollector::new();
        let top_docs_handle = collectors.add_collector(TopDocs::with_limit(size));
        let count_handle = collectors.add_collector(Count);

        let search_start = SystemTime::now();
        let mut multi_fruit = conver(searcher.search(&q, &collectors))?;

        let count = count_handle.extract(&mut multi_fruit);
        let top_docs = top_docs_handle.extract(&mut multi_fruit);
        let mut sdr = SearchDocumentResponse {
            code: Code::Success as i32,
            total: count as u64,
            hits: Vec::with_capacity(size),
            info: None, //if this is none means it is success
        };

        for (score, doc_address) in top_docs {
            let bytes_reader = searcher
                .segment_reader(doc_address.0)
                .fast_fields()
                .bytes(Field::from_field_id(ID_BYTES_INDEX))
                .unwrap();

            let doc = bytes_reader.get_bytes(doc_address.1);
            sdr.hits.push(Hit {
                collection_name: self.collection.name.to_string(),
                score: score,
                doc: doc.to_vec(),
            });
        }
        let search_finish = SystemTime::now();
        debug!(
            "search: merge result: cost({:?}ms)",
            search_finish
                .duration_since(search_start)
                .unwrap()
                .as_millis()
        );

        Ok(sdr)
    }

    pub fn exist(&self, iid: u32) -> ASResult<bool> {
        let searcher = self.index_reader.searcher();
        let query = TermQuery::new(
            Term::from_field_i64(Field::from_field_id(ID_INDEX), iid as i64),
            IndexRecordOption::Basic,
        );
        let td = TopDocs::with_limit(1);
        let result = conver(searcher.search(&query, &td))?;
        return Ok(result.len() > 0);
    }

    pub fn start_job(index: Arc<Tantivy>, receiver: Receiver<Event>) {
        std::thread::spawn(move || {
            let (cid, pid) = (index.base.collection.id, index.base.partition.id);
            Tantivy::index_job(index, receiver);
            warn!("collection:{}  partition:{} stop index job ", cid, pid);
        });
    }

    pub fn index_job(index: Arc<Tantivy>, rx: Receiver<Event>) {
        loop {
            let e = rx.recv();

            if e.is_err() {
                error!("revice err form index channel:{:?}", e.err());
                if index.base.runing() {
                    continue;
                } else {
                    return;
                }
            }

            let (old_iid, iid) = match e.unwrap() {
                Event::Delete(iid) => (iid, 0),
                Event::Update(old_iid, iid) => (old_iid, iid),
                Event::Stop => {
                    warn!("reviced stop event to stod index loop");
                    return;
                }
            };

            if iid == 0 {
                if old_iid > 0 {
                    if let Err(e) = index._delete(old_iid) {
                        error!("delete:{}  has err:{:?}", old_iid, e);
                    }
                }
            } else {
                match index.db.get_doc_by_id(iid_coding(iid)) {
                    Ok(v) => match v {
                        Some(v) => {
                            if let Err(e) = index._create(old_iid, iid, v) {
                                error!("index values has err:{:?}", e);
                            }
                        }
                        None => error!("not found doc by id:{}", iid),
                    },
                    Err(e) => {
                        error!("index get doc by db has err:{:?}", e);
                    }
                }
            }

            // set status to zero flush will check this value
            index.status.store(0, SeqCst);
        }
    }

    pub fn write(&self, event: Event) -> ASResult<()> {
        conver(self.tx.lock().unwrap().send(event))
    }

    fn _delete(&self, iid: u32) -> ASResult<()> {
        self.check_index()?;
        let ops = self
            .index_writer
            .read()
            .unwrap()
            .delete_term(Term::from_field_i64(Field::from_field_id(0), iid as i64));

        debug!("delete id:{} result:{:?}", iid, ops);
        Ok(())
    }

    fn _create(&self, old_iid: u32, iid: u32, value: Vec<u8>) -> ASResult<()> {
        self.check_index()?;
        let pbdoc: crate::pserverpb::Document =
            prost::Message::decode(prost::bytes::Bytes::from(value))?;

        let mut doc = Document::default();

        doc.add_i64(Field::from_field_id(ID_INDEX), iid as i64);
        doc.add_bytes(
            Field::from_field_id(ID_BYTES_INDEX),
            iid_coding(iid).to_vec(),
        );

        let source: serde_json::Value = serde_json::from_slice(pbdoc.source.as_slice())?;

        let mut flag: bool = false;

        for (f, fe) in self.index.schema().fields() {
            let v = &source[fe.name()];
            if v.is_null() {
                continue;
            }

            let array = self.collection.fields
                [self.collection.scalar_field_index[f.field_id() as usize - 2]]
                .array();

            if array {
                for a in v.as_array().unwrap() {
                    let v = match fe.field_type() {
                        &TantivyFT::Str(_) => Value::Str(a.as_str().unwrap().to_string()),
                        &TantivyFT::I64(_) => Value::I64(a.as_i64().unwrap()),
                        &TantivyFT::F64(_) => Value::F64(a.as_f64().unwrap()),
                        _ => {
                            return result!(
                                Code::FieldTypeErr,
                                "not support this type :{:?}",
                                fe.field_type(),
                            )
                        }
                    };
                    doc.add(FieldValue::new(f, v));
                }
            } else {
                let v = match fe.field_type() {
                    &TantivyFT::Str(_) => Value::Str(v.as_str().unwrap().to_string()),
                    &TantivyFT::I64(_) => Value::I64(v.as_i64().unwrap()),
                    &TantivyFT::F64(_) => Value::F64(v.as_f64().unwrap()),
                    _ => {
                        return result!(
                            Code::FieldTypeErr,
                            "not support this type :{:?}",
                            fe.field_type(),
                        )
                    }
                };
                doc.add(FieldValue::new(f, v));
            }

            flag = true;
        }
        let writer = self.index_writer.write().unwrap();
        if old_iid > 0 {
            writer.delete_term(Term::from_field_i64(
                Field::from_field_id(ID_INDEX),
                old_iid as i64,
            ));
        }
        if flag {
            writer.add_document(doc);
        }

        Ok(())
    }

    pub fn check_index(&self) -> ASResult<()> {
        if self.field_num <= 2 {
            return result!(Code::SpaceNoIndex, "space no index");
        }
        Ok(())
    }
}

impl Engine for Tantivy {
    fn flush(&self) -> ASResult<()> {
        if self.status.fetch_add(1, SeqCst) > 10 {
            return Ok(());
        }
        conver(self.index_writer.write().unwrap().commit())?;
        Ok(())
    }

    fn release(&self) {
        info!(
            "the collection:{} , partition:{} to release",
            self.partition.collection_id, self.partition.id
        );
        if let Err(e) = self.flush() {
            error!("flush engine has err:{:?}", e);
        }
    }
}
