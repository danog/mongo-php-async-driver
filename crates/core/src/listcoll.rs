//! `mongoc_database_find_collections_with_opts` exhausted into a Vec: the collinfo step of
//! automatic encryption (libmongoc's `_state_need_mongo_collinfo` on the encrypted client).

use crate::bson::writer::Writer;
use crate::client::Client;
use crate::cmdparts::{cursor_batch, Parts};
use crate::topology::OpType;
use crate::error::Error;

impl Client {
    /// `{listCollections: 1, filter}` on the primary, every result (getMore included)
    pub fn list_collections_all(&mut self, db: &str, filter: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let (sid, _) = self.select_server_id(OpType::Read, None, &[])?;
        let mut session = self.start_session(true, false).ok();
        let mut w = Writer::with_capacity(filter.len() + 48);
        w.int32(b"listCollections", 1);
        w.document(b"filter", filter);
        let body = w.finish();
        let r = (|| -> Result<Vec<Vec<u8>>, Error> {
            let mut parts = Parts::new(db, &body);
            parts.is_read = true;
            parts.session = session.as_mut();
            self.operation_id += 1;
            let mut c = self.assemble(parts, sid, false)?;
            let el = self.read_eligibility(&c, false, true);
            let reply = self.run_retryable_read(&mut c, session.as_mut(), el).map_err(|f| f.error);
            self.release_temp_session(&mut c);
            let reply = reply?;
            let mut out = Vec::new();
            let (mut id, mut batch) = cursor_batch(&reply, b"firstBatch");
            loop {
                out.append(&mut batch);
                if id == 0 {
                    return Ok(out);
                }
                let mut g = Writer::with_capacity(64);
                g.int64(b"getMore", id);
                g.utf8(b"collection", b"$cmd.listCollections");
                let gb = g.finish();
                let mut parts = Parts::new(db, &gb);
                parts.is_read = true;
                parts.session = session.as_mut();
                let mut c = self.assemble(parts, sid, false)?;
                let reply = self.run_monitored(&mut c, &[], session.as_mut()).map_err(|f| f.error);
                self.release_temp_session(&mut c);
                (id, batch) = cursor_batch(&reply?, b"nextBatch");
            }
        })();
        if let Some(mut s) = session.take() {
            self.end_session(&mut s);
        }
        r
    }
}
