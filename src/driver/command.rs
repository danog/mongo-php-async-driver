//! MongoDB\Driver\Command (src/MongoDB/Command.c)

use crate::bson::types::{cls, Cls, Props};
use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{hash, obj, sys, zval};
use mongo_core::bson::iter::{Iter, Value};
use std::ptr::{addr_of_mut, null_mut};

#[derive(Default)]
pub struct Command {
    pub bson: Option<Vec<u8>>,
    pub batch_size: u32,
    pub max_await_time_ms: u32,
    props: Props,
}
cls!(Command, H_COMMAND);

impl Cls for Command {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.props
    }
    fn dup(&self) -> Self {
        Command { bson: self.bson.clone(), batch_size: self.batch_size, max_await_time_ms: self.max_await_time_ms, props: Props::default() }
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, _debug: bool) -> bool {
        match &self.bson {
            Some(b) => match super::to_zval(b, &Default::default()) {
                Some(z) => hash::str_update(ht, b"command", z),
                None => return false,
            },
            None => hash::str_update(ht, b"command", zval::null()),
        }
        true
    }
}

pub static mut CE: *mut sys::zend_class_entry = null_mut();

pub unsafe fn init_MongoDB_Driver_Command(c: *mut sys::zend_class_entry) {
    CE = c;
    let h = crate::bson::types::setup::<Command>(c);
    // phongo: debug info only (no get_properties, no clone)
    h.get_properties = sys::std_object_handlers.get_properties;
    h.clone_obj = None;
}

/// The command's first key (its name).
pub fn name(bson: &[u8]) -> String {
    Iter::new(bson).and_then(|mut it| it.next().then(|| String::from_utf8_lossy(it.key()).into_owned())).unwrap_or_default()
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Command___construct(ex: Ex, _rv: *mut sys::zval) {
    let mut doc: *mut sys::zval = null_mut();
    let mut opts: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"z|a!".as_ptr(), &mut doc, &mut opts) != 0 {
        return;
    }
    if !zval::is(doc, sys::IS_ARRAY) && !zval::is(doc, sys::IS_OBJECT) {
        sys::zend_argument_type_error(1, c"must be of type object|array, %s given".as_ptr(), sys::zend_zval_value_name(doc));
        return;
    }
    let me = obj::data::<Command>(call::this(ex));
    let Some(b) = crate::bson::encode::to_bson(doc, crate::bson::encode::NONE) else { return };
    // cursor.batchSize of the command
    if let Some(mut it) = Iter::new(&b) {
        if it.find(b"cursor") {
            if let Value::Document(d) = it.value() {
                if let Some(mut c) = Iter::new(d) {
                    if c.find(b"batchSize") {
                        if let v @ (Value::Int32(_) | Value::Int64(_)) = c.value() {
                            let n = mongo_core::sdam::as_i64(&v);
                            if (0..=u32::MAX as i64).contains(&n) {
                                me.batch_size = n as u32;
                            }
                        }
                    }
                }
            }
        }
    }
    me.bson = Some(b);
    if !opts.is_null() {
        if let Some(z) = hash::find((*opts).value.arr, b"maxAwaitTimeMS") {
            let v = sys::zval_get_long_func(z, false);
            if v < 0 {
                throw(E::InvalidArgument, format!("Expected \"maxAwaitTimeMS\" option to be >= 0, {v} given"));
                return;
            }
            if v > u32::MAX as i64 {
                throw(E::InvalidArgument, format!("Expected \"maxAwaitTimeMS\" option to be <= {}, {v} given", u32::MAX));
                return;
            }
            me.max_await_time_ms = v as u32;
        }
    }
}

#[allow(dead_code)]
fn _h() -> *mut sys::zend_object_handlers {
    addr_of_mut!(H_COMMAND)
}
