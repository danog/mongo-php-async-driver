//! MongoDB\Driver\Session (src/MongoDB/Session.c over mongoc_client_session_t).

use super::ClientRef;
use crate::zend::{obj, sys};
use mongo_core::session::ClientSession;
use std::cell::RefCell;
use std::ptr::{addr_of_mut, null_mut};
use std::rc::Rc;

#[derive(Default)]
pub struct Session {
    pub cs: Option<Rc<RefCell<ClientSession>>>,
    pub client: Option<ClientRef>,
}

static mut H_SESSION: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, Session::default(), addr_of_mut!(H_SESSION))
}

pub unsafe fn init_MongoDB_Driver_Session(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    obj::handlers::<Session>(addr_of_mut!(H_SESSION));
}

/// The Session state of the object in `z`.
pub unsafe fn of<'a>(z: *mut sys::zval) -> &'a mut Session {
    obj::data::<Session>((*z).value.obj)
}
