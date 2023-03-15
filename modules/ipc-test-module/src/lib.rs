include!(concat!(env!("OUT_DIR"), "/simics_module_header.rs"));

pub mod messages;

use crate::messages::{FuzzerEvent, Message, SimicsEvent};
use anyhow::{bail, Result};
use confuse_simics_api::{
    class_data_t, class_kind_t_Sim_Class_Kind_Session, conf_class, SIM_register_class,
};
use const_format::concatcp;
use ipc_channel::ipc::{channel, IpcReceiver, IpcSender};
use ipc_shm::{IpcShm, IpcShmWriter};
use lazy_static::lazy_static;
use log::info;

use std::{
    env::var,
    ffi::CString,
    sync::{Arc, Mutex},
};

pub const BOOTSTRAP_SOCKNAME: &str = concatcp!(CLASS_NAME, "_SOCK");
pub const AFL_MAPSIZE: usize = 64 * 1024;

pub struct ModuleCtx {
    _cls: *mut conf_class,
    _tx: IpcSender<Message>,
    _rx: IpcReceiver<Message>,
    _shm: IpcShm,
    _writer: IpcShmWriter,
}

unsafe impl Send for ModuleCtx {}
unsafe impl Sync for ModuleCtx {}

impl ModuleCtx {
    pub fn try_new(cls: *mut conf_class) -> Result<Self> {
        let bootstrap = IpcSender::connect(var(BOOTSTRAP_SOCKNAME)?)?;

        info!("Bootstrapped connection for IPC");

        let (otx, rx) = channel::<Message>()?;
        let (tx, orx) = channel::<Message>()?;

        info!("Sending fuzzer IPC channel");

        bootstrap.send((otx, orx))?;

        info!("Waiting for initialize command");

        match rx.recv()? {
            Message::FuzzerEvent(FuzzerEvent::Initialize) => {}
            _ => bail!("Expected initialize command"),
        };

        let mut shm = IpcShm::default();

        let mut writer = shm.writer()?;

        for i in 0..writer.len() {
            writer.write_at(&[(i % u8::MAX as usize) as u8], i)?;
        }

        info!("Sending fuzzer memory map");

        tx.send(Message::SimicsEvent(SimicsEvent::SharedMem(
            shm.try_clone()?,
        )))?;

        Ok(Self {
            _cls: cls,
            _tx: tx,
            _rx: rx,
            _shm: shm,
            _writer: writer,
        })
    }
}

lazy_static! {
    static ref CTX: Arc<Mutex<ModuleCtx>> = {
        let class_name: CString = CString::new(CLASS_NAME).expect("CString::new failed");
        let class_data_desc = CString::new("Minimal module").expect("CString::new failed");
        let class_data_class_desc =
            CString::new("Minimal module class").expect("CString::new failed");

        let class_data = class_data_t {
            alloc_object: None,
            init_object: None,
            finalize_instance: None,
            pre_delete_instance: None,
            delete_instance: None,
            // Leaked
            description: class_data_desc.into_raw(),
            // Leaked
            class_desc: class_data_class_desc.into_raw(),
            kind: class_kind_t_Sim_Class_Kind_Session,
        };

        let cls = unsafe {
            // Class name Leaked
            SIM_register_class(class_name.into_raw(), &class_data as *const class_data_t)
        };

        Arc::new(Mutex::new(
            ModuleCtx::try_new(cls).expect("Failed to initialize module"),
        ))
    };
}

#[no_mangle]
pub extern "C" fn init_local() {
    let _ctx = CTX.lock().expect("Could not lock context!");
    info!("Initialized context for {}", CLASS_NAME);
}
