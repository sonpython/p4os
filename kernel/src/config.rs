//! Persistent key/value settings in SPI flash.
//!
//! Two sectors at the start of the kernel's flash region hold alternating
//! records (format in `p4os_sdk::config`). A save goes to the sector not
//! holding the current record and carries the next sequence number; the
//! old record is only superseded once the new one has been written and
//! read back intact. Power loss during a save therefore leaves either the
//! old or the new settings, never a mix.

use crate::sync::Mutex;
use crate::{flash, kprintln};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use p4os_sdk::config::{self as format, Entries};

const SLOTS: [u32; 2] = [flash::REGION.0, flash::REGION.0 + flash::SECTOR];

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // fields are read through Debug when an error is logged
pub enum Error {
    /// Flash was not usable at boot, so nothing can be saved.
    Unavailable,
    Flash(flash::Error),
    Format(format::Error),
    /// The record read back differs from what was written.
    Verify,
}

struct Store {
    entries: Entries,
    seq: u32,
    /// Sector holding the current record, if any.
    slot: Option<usize>,
    available: bool,
}

static STORE: Mutex<Store> = Mutex::new(Store { entries: BTreeMap::new(), seq: 0, slot: None, available: false });

/// Load the newest valid record. Called once at boot, before threads run.
pub fn init() {
    if let Err(e) = flash::rom_ready() {
        kprintln!("config: flash unavailable ({:?}), settings will not persist", e);
        return;
    }
    let mut records = [None, None];
    let mut sector = vec![0u8; flash::SECTOR as usize];
    for (i, &addr) in SLOTS.iter().enumerate() {
        match flash::read(addr, &mut sector) {
            Ok(()) => records[i] = format::decode(&sector),
            Err(e) => kprintln!("config: read {:#x} failed: {:?}", addr, e),
        }
    }
    let mut s = STORE.lock();
    s.available = true;
    match format::newest(&records) {
        Some(i) => {
            let r = records[i].take().unwrap();
            kprintln!("config: {} keys (sector {:#x}, seq {})", r.entries.len(), SLOTS[i], r.seq);
            s.entries = r.entries;
            s.seq = r.seq;
            s.slot = Some(i);
        }
        None => kprintln!("config: empty"),
    }
}

pub fn get(key: &str) -> Option<String> {
    STORE.lock().entries.get(key).cloned()
}

pub fn entries() -> Entries {
    STORE.lock().entries.clone()
}

pub fn set(key: &str, value: &str) -> Result<(), Error> {
    update(|e| {
        e.insert(key.into(), value.into());
    })
}

/// Apply `f` to a copy of the settings and save the result atomically.
/// Nothing is written when `f` changes nothing.
pub fn update(f: impl FnOnce(&mut Entries)) -> Result<(), Error> {
    let mut s = STORE.lock();
    let mut next = s.entries.clone();
    f(&mut next);
    if next == s.entries {
        return Ok(());
    }
    if !s.available {
        return Err(Error::Unavailable);
    }
    let seq = s.seq.wrapping_add(1);
    let record = format::encode(seq, &next, flash::SECTOR as usize).map_err(Error::Format)?;
    let target = s.slot.map_or(0, |i| 1 - i);
    let addr = SLOTS[target];
    flash::erase_sector(addr).map_err(Error::Flash)?;
    flash::write(addr, &record).map_err(Error::Flash)?;
    let mut back = vec![0u8; record.len()];
    flash::read(addr, &mut back).map_err(Error::Flash)?;
    if back != record {
        return Err(Error::Verify);
    }
    s.entries = next;
    s.seq = seq;
    s.slot = Some(target);
    Ok(())
}
