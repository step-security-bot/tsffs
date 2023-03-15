use anyhow::{bail, ensure, Context, Result};
use confuse_fuzz::message::{FuzzerEvent, SimicsEvent, StopType};
use confuse_fuzz::{Fault, InitInfo};
use confuse_simics_api::{
    attr_attr_t_Sim_Attr_Pseudo, class_data_t, class_kind_t_Sim_Class_Kind_Session, conf_class,
    conf_class_t, micro_checkpoint_flags_t_Sim_MC_ID_User,
    micro_checkpoint_flags_t_Sim_MC_Persistent, physical_address_t, CORE_discard_future,
    SIM_attr_list_size, SIM_continue, SIM_get_attribute, SIM_get_object, SIM_hap_add_callback,
    SIM_register_attribute, SIM_register_class, SIM_run_alone, SIM_write_phys_memory,
    VT_restore_micro_checkpoint, VT_save_micro_checkpoint,
};

use ipc_channel::ipc::{channel, IpcReceiver, IpcSender};
use ipc_shm::{IpcShm, IpcShmWriter};
use lazy_static::lazy_static;
use log::info;
use raw_cstr::raw_cstr;

use crate::callbacks::{
    core_exception_cb, core_simulation_stopped_cb, get_processor, get_signal, set_processor,
    set_signal, x86_triple_fault_cb,
};

use crate::magic::Magic;
use crate::processor::Processor;
use crate::signal::Signal;
use crate::stop_reason::StopReason;
use crate::{
    callbacks::core_magic_instruction_cb,
    interface::{BOOTSTRAP_SOCKNAME, CLASS_NAME},
};

use std::collections::HashSet;
use std::{
    env::var,
    ffi::CString,
    mem::transmute,
    ptr::null_mut,
    sync::{Arc, Mutex},
};

/// Container for the SIMICS structures needed to trace execution of a processor
/// Context for the module. This module is responsible for:
/// - Handling messages from SIMICS
/// - Branch tracing
/// - Detecting errors
pub struct ModuleCtx {
    _cls: *mut conf_class,
    tx: IpcSender<SimicsEvent>,
    rx: IpcReceiver<FuzzerEvent>,
    _shm: IpcShm,
    writer: IpcShmWriter,
    processor: Option<Processor>,
    stop_reason: Option<StopReason>,
    initialized: bool,
    prev_loc: u64,
    buffer_address: u64,
    buffer_size: u64,
    init_info: InitInfo,
}

unsafe impl Send for ModuleCtx {}
unsafe impl Sync for ModuleCtx {}

impl ModuleCtx {
    pub fn try_new(cls: *mut conf_class) -> Result<Self> {
        let bootstrap = IpcSender::connect(var(BOOTSTRAP_SOCKNAME)?)?;

        info!("Bootstrapped connection for IPC");

        let (otx, rx) = channel::<FuzzerEvent>()?;
        let (tx, orx) = channel::<SimicsEvent>()?;

        info!("Sending fuzzer IPC channel");

        bootstrap.send((otx, orx))?;

        info!("Waiting for initialize command");

        let init_info = match rx.recv()? {
            FuzzerEvent::Initialize(info) => info,
            _ => bail!("Expected initialize command"),
        };

        if init_info.faults.contains(&Fault::Triple) {
            // We care about triple, set the x86 triple handler
            let _triple_fault_cb = unsafe {
                SIM_hap_add_callback(
                    raw_cstr!("X86_Triple_Fault"),
                    transmute(x86_triple_fault_cb as unsafe extern "C" fn(_, _)),
                    null_mut(),
                )
            };
        }

        if init_info
            .faults
            .difference(&HashSet::from([Fault::Triple]))
            .next()
            .is_some()
        {
            // 1+ elements that aren't triple, we need to set the core_exception handler
            let _triple_fault_cb = unsafe {
                SIM_hap_add_callback(
                    raw_cstr!("Core_Exception"),
                    transmute(core_exception_cb as unsafe extern "C" fn(_, _, _)),
                    null_mut(),
                )
            };
        }

        let mut shm = IpcShm::default();

        let mut writer = shm.writer()?;

        for i in 0..writer.len() {
            writer.write_at(&[(i % u8::MAX as usize) as u8], i)?;
        }

        info!("Sending fuzzer memory map");

        tx.send(SimicsEvent::SharedMem(shm.try_clone()?))?;

        Ok(Self {
            _cls: cls,
            tx,
            rx,
            _shm: shm,
            writer,
            processor: None,
            stop_reason: None,
            initialized: false,
            prev_loc: 0,
            buffer_address: 0,
            buffer_size: 0,
            init_info,
        })
    }

    pub fn init(&mut self) -> Result<()> {
        info!("Initialized Module Context");

        Ok(())
    }

    pub fn set_processor(&mut self, processor: Processor) -> Result<()> {
        self.processor = Some(processor);
        Ok(())
    }

    pub fn get_processor(&self) -> Result<&Processor> {
        self.processor.as_ref().context("No processor available")
    }

    pub fn handle_signal(&self, signal: Signal) {
        if matches!(signal, Signal::Start) {
            self.start()
        }
    }

    pub unsafe fn resume_simulation(&self) {
        SIM_run_alone(
            Some(transmute(SIM_continue as unsafe extern "C" fn(_) -> _)),
            null_mut(),
        );
    }

    pub fn handle_stop(&mut self) -> Result<()> {
        match &self.stop_reason {
            Some(StopReason::Magic(m)) => match m {
                Magic::Start => {
                    if self.initialized {
                        // If we're already initialized, so we just go
                        info!("Got start magic. Already initialized, off we go!");
                        unsafe { self.resume_simulation() };
                    } else {
                        // Start harness stop means we need to take a snapshot!
                        unsafe {
                            VT_save_micro_checkpoint(
                                raw_cstr!("origin"),
                                micro_checkpoint_flags_t_Sim_MC_ID_User
                                    | micro_checkpoint_flags_t_Sim_MC_Persistent,
                            )
                        };

                        info!("Took snapshot");

                        let processor = self.get_processor()?;
                        let cpu = processor.get_cpu();
                        info!("Got processor");
                        let rsi_number = unsafe {
                            (*processor.get_int_register())
                                .get_number
                                .context("No function get_number")?(
                                cpu, raw_cstr!("rsi")
                            )
                        };

                        info!("Got number for register rsi: {}", rsi_number);

                        let rdi_number = unsafe {
                            (*processor.get_int_register())
                                .get_number
                                .context("No function get_number")?(
                                cpu, raw_cstr!("rdi")
                            )
                        };

                        info!("Got number for register rdi: {}", rdi_number);

                        let rsi_value = unsafe {
                            (*processor.get_int_register())
                                .read
                                .context("No read function available")?(
                                cpu, rsi_number
                            )
                        };

                        info!("Got value for register rsi: {:#x}", rsi_value);

                        let rdi_value = unsafe {
                            (*processor.get_int_register())
                                .read
                                .context("No read function available")?(
                                cpu, rdi_number
                            )
                        };

                        info!("Got value for register rdi: {:#x}", rdi_value);

                        self.buffer_address = rsi_value;
                        self.buffer_size = rdi_value;

                        self.initialized = true;

                        self.tx.send(SimicsEvent::Ready)?;

                        info!("Sent ready signal");

                        // We'll wait for a signal to start
                        match self.rx.recv()? {
                            FuzzerEvent::Run(input) => {
                                info!("Got input, running");

                                for (i, chunk) in input.chunks(8).enumerate() {
                                    // TODO: this is really inefficient, make it better
                                    let data: &mut [u8] = &mut [0; 8];
                                    for (i, v) in chunk.iter().enumerate() {
                                        data[i] = *v;
                                    }
                                    let val = u64::from_le_bytes(data.try_into()?);
                                    let addr: physical_address_t =
                                        self.buffer_address + (i * 8) as u64;
                                    unsafe {
                                        SIM_write_phys_memory(
                                            cpu,
                                            addr,
                                            val,
                                            chunk.len().try_into()?,
                                        )
                                    };
                                }
                                unsafe { self.resume_simulation() };
                            }
                            _ => {
                                bail!("Unexpected event");
                            }
                        }
                    }
                }
                Magic::Stop => {
                    // Stop harness stop means we need to reset to the snapshot and be ready to
                    // run
                    self.tx.send(SimicsEvent::Stopped(StopType::Normal))?;

                    let rexec = unsafe { SIM_get_object(raw_cstr!("sim.rexec")) };

                    let sinfo = unsafe { SIM_get_attribute(rexec, raw_cstr!("state_info")) };

                    let sinfo_size = SIM_attr_list_size(sinfo)?;

                    ensure!(
                        sinfo_size == 1,
                        "Invalid size of state_info: {}",
                        sinfo_size
                    );
                    info!("Waiting for reset signal to restore state");

                    match self.rx.recv()? {
                        FuzzerEvent::Reset => {
                            unsafe { VT_restore_micro_checkpoint(0) };
                            unsafe { CORE_discard_future() };

                            info!("Restored checkpoint");
                        }
                        FuzzerEvent::Stop => {
                            info!("Got stop signal, we want to stop cleanly here");
                        }
                        _ => {
                            bail!("Unexpected event");
                        }
                    }

                    self.tx.send(SimicsEvent::Ready)?;

                    let processor = self.get_processor()?;
                    let cpu = processor.get_cpu();

                    match self.rx.recv()? {
                        FuzzerEvent::Run(input) => {
                            info!("Got input, running");

                            for (i, chunk) in input.chunks(8).enumerate() {
                                // TODO: this is really inefficient, make it better
                                let data: &mut [u8] = &mut [0; 8];
                                for (i, v) in chunk.iter().enumerate() {
                                    data[i] = *v;
                                }
                                let val = u64::from_le_bytes(data.try_into()?);
                                let addr: physical_address_t = self.buffer_address + (i * 8) as u64;
                                unsafe {
                                    SIM_write_phys_memory(cpu, addr, val, chunk.len().try_into()?)
                                };
                            }
                            unsafe { self.resume_simulation() };
                        }
                        _ => {
                            bail!("Unexpected event");
                        }
                    }
                }
            },
            Some(StopReason::Crash) => {
                self.tx.send(SimicsEvent::Stopped(StopType::Crash))?;
                let rexec = unsafe { SIM_get_object(raw_cstr!("sim.rexec")) };

                let sinfo = unsafe { SIM_get_attribute(rexec, raw_cstr!("state_info")) };

                let sinfo_size = SIM_attr_list_size(sinfo)?;

                ensure!(
                    sinfo_size == 1,
                    "Invalid size of state_info: {}",
                    sinfo_size
                );
                info!("Waiting for reset signal to restore state");

                match self.rx.recv()? {
                    FuzzerEvent::Reset => {
                        unsafe { VT_restore_micro_checkpoint(0) };
                        unsafe { CORE_discard_future() };

                        info!("Restored checkpoint");
                    }
                    FuzzerEvent::Stop => {
                        info!("Got stop signal, we want to stop cleanly here");
                    }
                    _ => {
                        bail!("Unexpected event");
                    }
                }

                self.tx.send(SimicsEvent::Ready)?;

                let processor = self.get_processor()?;
                let cpu = processor.get_cpu();

                match self.rx.recv()? {
                    FuzzerEvent::Run(input) => {
                        info!("Got input, running");

                        for (i, chunk) in input.chunks(8).enumerate() {
                            // TODO: this is really inefficient, make it better
                            let data: &mut [u8] = &mut [0; 8];
                            for (i, v) in chunk.iter().enumerate() {
                                data[i] = *v;
                            }
                            let val = u64::from_le_bytes(data.try_into()?);
                            let addr: physical_address_t = self.buffer_address + (i * 8) as u64;
                            unsafe {
                                SIM_write_phys_memory(cpu, addr, val, chunk.len().try_into()?)
                            };
                        }
                        unsafe { self.resume_simulation() };
                    }
                    _ => {
                        bail!("Unexpected event");
                    }
                }
            }
            None => {}
        }

        self.stop_reason = None;

        Ok(())
    }

    pub fn set_stopped_reason(&mut self, reason: Option<StopReason>) -> Result<()> {
        self.stop_reason = reason;
        Ok(())
    }

    pub fn start(&self) {
        info!("Starting module");
        unsafe { self.resume_simulation() };
    }

    pub fn log(&mut self, pc: u64) -> Result<()> {
        let cur_loc = ((pc >> 4) ^ (pc << 8)) & (self.writer.len() - 1) as u64;
        let data = &[self.writer.read_byte(cur_loc as usize)?];
        self.writer
            .write_at(data, (cur_loc ^ self.prev_loc) as usize)?;
        self.prev_loc >>= 1;
        Ok(())
    }

    pub fn is_fault(&self, fault: Fault) -> bool {
        self.init_info.faults.contains(&fault)
    }
}

lazy_static! {
    pub static ref CTX: Arc<Mutex<ModuleCtx>> = {

        // reference-manual-api/device-api-data-types.html
        let class_data = class_data_t {
            alloc_object: None,
            init_object: None,
            finalize_instance: None,
            pre_delete_instance: None,
            delete_instance: None,
            // Leaked
            description: raw_cstr!(CLASS_NAME),
            // Leaked
            class_desc: raw_cstr!("Confuse module"),
            kind: class_kind_t_Sim_Class_Kind_Session,
        };

        let cls: *mut conf_class_t  = unsafe {
            // Class name Leaked
            SIM_register_class(raw_cstr!(CLASS_NAME), &class_data as *const class_data_t)
        };

        unsafe {
            SIM_register_attribute(
                cls,
                raw_cstr!("processor"),
                Some(get_processor),
                Some(set_processor),
                attr_attr_t_Sim_Attr_Pseudo,
                // https://docs.python.org/3/c-api/arg.html#parsing-arguments
                raw_cstr!("o|n"),
                raw_cstr!("The <i>processor</i> to trace."),
            );
        };

        unsafe {
            SIM_register_attribute(
                cls,
                raw_cstr!("signal"),
                Some(get_signal),
                Some(set_signal),
                attr_attr_t_Sim_Attr_Pseudo,
                raw_cstr!("i"),
                raw_cstr!("Pseudo interface for sending a signal"),
            );
        };

        info!("Registered processor attribute");

        let _magic_cb_handle = unsafe {
            SIM_hap_add_callback(
                raw_cstr!("Core_Magic_Instruction"),
                transmute(core_magic_instruction_cb as unsafe extern "C" fn(_, _, _)),
                null_mut(),
            )
        };

        let _stop_cb_handle = unsafe {
            SIM_hap_add_callback(
                raw_cstr!("Core_Simulation_Stopped"),
                transmute(core_simulation_stopped_cb as unsafe extern "C" fn(_, _, _, _)),
                null_mut(),
            )
        };


        info!("Added callback for magic instruction");


        Arc::new(Mutex::new(
            ModuleCtx::try_new(cls).expect("Failed to initialize module"),
        ))
    };
}
