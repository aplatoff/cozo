use crate::data::attr::Attribute;
use crate::data::encode::{encode_tx, encode_unique_attr_by_id, encode_unique_entity, EncodedVec};
use crate::data::id::{AttrId, EntityId, TxId};
use crate::data::keyword::Keyword;
use crate::data::value::StaticValue;
use anyhow::Result;
use cozorocks::{DbIter, Tx};
use rmp_serde::Serializer;
use serde::Serialize;
use serde_derive::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) struct SessionTx {
    pub(crate) tx: Tx,
    pub(crate) r_tx_id: TxId,
    pub(crate) w_tx_id: Option<TxId>,
    pub(crate) last_attr_id: Arc<AtomicU64>,
    pub(crate) last_ent_id: Arc<AtomicU64>,
    pub(crate) last_tx_id: Arc<AtomicU64>,
    pub(crate) attr_by_id_cache: BTreeMap<AttrId, Option<Attribute>>,
    pub(crate) attr_by_kw_cache: BTreeMap<Keyword, Option<Attribute>>,
    pub(crate) temp_entity_to_perm: BTreeMap<EntityId, EntityId>,
    pub(crate) eid_by_attr_val_cache: BTreeMap<StaticValue, BTreeMap<AttrId, Option<EntityId>>>,
    // "touched" requires the id to exist prior to the transaction, and something related to it has changed
    pub(crate) touched_eids: BTreeSet<EntityId>,
}

#[derive(Clone, PartialEq, Ord, PartialOrd, Eq, Debug, Deserialize, Serialize)]
pub(crate) struct TxLog {
    #[serde(rename = "i")]
    pub(crate) id: TxId,
    #[serde(rename = "c")]
    pub(crate) comment: String,
    #[serde(rename = "t")]
    pub(crate) timestamp: i64,
}

impl TxLog {
    pub(crate) fn new(id: TxId, comment: &str) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        Self {
            id,
            comment: comment.to_string(),
            timestamp,
        }
    }
    pub(crate) fn encode(&self) -> EncodedVec<64> {
        let mut store = SmallVec::<[u8; 64]>::new();
        self.serialize(&mut Serializer::new(&mut store)).unwrap();
        EncodedVec { inner: store }
    }
    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        Ok(rmp_serde::from_slice(data)?)
    }
}

impl SessionTx {
    pub(crate) fn clear_cache(&mut self) {
        self.attr_by_id_cache.clear();
        self.attr_by_kw_cache.clear();
        self.temp_entity_to_perm.clear();
        self.touched_eids.clear();
        self.eid_by_attr_val_cache.clear();
    }

    pub(crate) fn load_last_entity_id(&mut self) -> Result<EntityId> {
        let e_lower = encode_unique_entity(EntityId::MIN_PERM);
        let e_upper = encode_unique_entity(EntityId::MAX_PERM);
        let it = self.bounded_scan_last(&e_lower, &e_upper);

        Ok(match it.key()? {
            None => EntityId::MAX_TEMP,
            Some(data) => EntityId::from_bytes(data),
        })
    }

    pub(crate) fn load_last_attr_id(&mut self) -> Result<AttrId> {
        let e_lower = encode_unique_attr_by_id(AttrId::MIN_PERM);
        let e_upper = encode_unique_attr_by_id(AttrId::MAX_PERM);
        let it = self.bounded_scan_last(&e_lower, &e_upper);
        Ok(match it.key()? {
            None => AttrId::MAX_TEMP,
            Some(data) => AttrId::from_bytes(data),
        })
    }

    pub(crate) fn load_last_tx_id(&mut self) -> Result<TxId> {
        let e_lower = encode_tx(TxId::MAX_USER);
        let e_upper = encode_tx(TxId::MAX_SYS);
        let it = self.bounded_scan_first(&e_lower, &e_upper);
        Ok(match it.key()? {
            None => TxId::MAX_SYS,
            Some(data) => TxId::from_bytes(data),
        })
    }

    pub(crate) fn commit_tx(&mut self, comment: &str, refresh: bool) -> Result<()> {
        let tx_id = self.get_write_tx_id()?;
        let encoded = encode_tx(tx_id);

        let log = TxLog::new(tx_id, comment);
        self.tx.put(&encoded, &log.encode())?;
        self.tx.commit()?;
        if refresh {
            let new_tx_id = TxId(self.last_tx_id.fetch_add(1, Ordering::AcqRel) + 1);
            self.tx.set_snapshot();
            self.r_tx_id = new_tx_id;
            self.w_tx_id = Some(new_tx_id);
            self.clear_cache();
        }
        Ok(())
    }

    pub(crate) fn get_write_tx_id(&self) -> std::result::Result<TxId, TransactError> {
        self.w_tx_id.ok_or(TransactError::WriteInReadOnly)
    }
    pub(crate) fn bounded_scan(&mut self, lower: &[u8], upper: &[u8]) -> DbIter {
        self.tx
            .iterator()
            .lower_bound(lower)
            .lower_bound(upper)
            .start()
    }
    pub(crate) fn bounded_scan_first(&mut self, lower: &[u8], upper: &[u8]) -> DbIter {
        let mut it = self.tx.iterator().upper_bound(upper).start();
        it.seek(lower);
        it
    }

    pub(crate) fn bounded_scan_last(&mut self, lower: &[u8], upper: &[u8]) -> DbIter {
        let mut it = self.tx.iterator().lower_bound(lower).start();
        it.seek_back(upper);
        it
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransactError {
    #[error("attribute conflict for {0:?}: {1}")]
    AttrConflict(AttrId, String),
    #[error("attribute consistency for {0:?}: {1}")]
    AttrConsistency(AttrId, String),
    #[error("attribute not found {0:?}")]
    AttrNotFound(AttrId),
    #[error("attempt to write in read-only transaction")]
    WriteInReadOnly,
    #[error("attempt to change immutable property for attr {0:?}")]
    ChangingImmutableProperty(AttrId),
}
