//! Battery readers and unified `.bss`-backed HID + Bluetooth scanner (`#![no_std]`, zero heap).

use core::ffi::c_void;
use core::ptr::{addr_of_mut, null, null_mut};

use crate::drivers::{
    bluetooth, corsair, hyperx,
    io::{open_hid, open_hid_feature, vp_u32, RawCandidate},
    logitech::{self, LogitechEndpoint, LOGI_EP_CAP},
    pulsar, razer, steelseries,
};
use crate::win32::{
    CloseHandle, CreateEventW, GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST,
    RIDI_DEVICEINFO, RIDI_DEVICENAME, RID_DEVICE_INFO, RIM_TYPEHID, HANDLE,
};

pub use crate::drivers::io::{
    probe_product_string_utf16, voltage_to_pct, HidPath, RawCandidate as RawHidCandidate,
};
pub use crate::drivers::logitech::{
    hidpp_request, parse_1000_status, parse_1001_voltage, parse_1004_status, HIDPP_SW_ID,
};
pub use crate::drivers::hyperx::{parse_hyperx_battery, parse_hyperx_charging};

pub const MAX_DEVICES: usize = 6;
pub const MAX_PRESENT_VP: usize = 24;
const RAW_LIST_CAP: usize = 128;
const CAND_CAP: usize = 24;
const GEN_EP_CAP: usize = 8;
const BT_ID_BUF_CAP: usize = 8192;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
#[repr(u8)]
pub enum Kind {
    Mouse = 1,
    Keyboard = 2,
    Headset = 3,
    Gamepad = 4,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Source {
    NativeHid = 1,
    Bluetooth = 2,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Battery {
    pub id: u64,
    pub vid_pid: u32,
    pub kind: Kind,
    pub pct: Option<u8>,
    pub charging: bool,
    pub online: bool,
    pub source: Source,
}

impl Battery {
    pub const fn new(kind: Kind, pct: Option<u8>, charging: bool, online: bool) -> Self {
        Self {
            id: kind as u64,
            vid_pid: 0,
            kind,
            pct,
            charging,
            online,
            source: Source::NativeHid,
        }
    }

    pub const fn with_id(
        id: u64,
        vid_pid: u32,
        kind: Kind,
        pct: Option<u8>,
        charging: bool,
        online: bool,
        source: Source,
    ) -> Self {
        Self {
            id,
            vid_pid,
            kind,
            pct,
            charging,
            online,
            source,
        }
    }
}

/// Deterministic multi-device container sorted by `(kind, id)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BatteryList {
    pub items: [Option<Battery>; MAX_DEVICES],
    pub len: usize,
}

impl Default for BatteryList {
    fn default() -> Self {
        Self::new()
    }
}

impl BatteryList {
    pub const fn new() -> Self {
        Self {
            items: [None; MAX_DEVICES],
            len: 0,
        }
    }

    fn sort(&mut self) {
        let n = self.len.min(MAX_DEVICES);
        for i in 1..n {
            let mut j = i;
            while j > 0 {
                let (Some(a), Some(b)) = (self.items[j - 1], self.items[j]) else {
                    break;
                };
                if (b.kind, b.id) < (a.kind, a.id) {
                    self.items.swap(j - 1, j);
                    j -= 1;
                } else {
                    break;
                }
            }
        }
    }

    /// Insert or update a device reading, preferring `Source::NativeHid` over `Source::Bluetooth`
    /// when both expose the same `(vid_pid, kind)`.
    pub fn upsert(&mut self, b: Battery) {
        let n = self.len.min(MAX_DEVICES);
        for slot in &mut self.items[..n] {
            let Some(existing) = *slot else { continue };
            let same_id = existing.id == b.id;
            let same_hw = b.vid_pid != 0
                && existing.vid_pid == b.vid_pid
                && existing.kind == b.kind
                && existing.source != b.source;
            if same_id || same_hw {
                if existing.source == Source::NativeHid && b.source == Source::Bluetooth {
                    return;
                }
                *slot = Some(b);
                self.sort();
                return;
            }
        }
        if self.len < MAX_DEVICES {
            self.items[self.len] = Some(b);
            self.len += 1;
            self.sort();
        }
    }

    pub fn set(&mut self, b: Battery) {
        self.upsert(b);
    }

    pub fn get(&self, kind: Kind) -> Option<&Battery> {
        self.items[..self.len]
            .iter()
            .flatten()
            .find(|b| b.kind == kind)
    }

    pub fn get_by_id(&self, id: u64) -> Option<&Battery> {
        self.items[..self.len]
            .iter()
            .flatten()
            .find(|b| b.id == id)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn count(&self) -> usize {
        self.len
    }

    pub fn any_charging(&self) -> bool {
        self.items[..self.len]
            .iter()
            .flatten()
            .any(|b| b.charging)
    }

    pub fn as_slice(&self) -> &[Option<Battery>] {
        &self.items[..self.len]
    }

    pub fn iter(&self) -> impl Iterator<Item = &Battery> {
        self.items[..self.len].iter().flatten()
    }
}

#[derive(Clone, Copy)]
pub struct PollOutput {
    pub devices: BatteryList,
    pub present_hid_vp: [u32; MAX_PRESENT_VP],
    pub present_hid_vp_len: usize,
}

impl Default for PollOutput {
    fn default() -> Self {
        Self::empty()
    }
}

impl PollOutput {
    pub const fn empty() -> Self {
        Self {
            devices: BatteryList::new(),
            present_hid_vp: [0; MAX_PRESENT_VP],
            present_hid_vp_len: 0,
        }
    }
}

#[derive(Clone, Copy)]
enum GenericProto {
    None,
    Hyperx(hyperx::HyperxProto),
    Razer(Kind, u8),
    Steel(steelseries::SteelProto),
    Corsair(corsair::CorsairProto),
    Pulsar,
}

#[derive(Clone, Copy)]
struct GenericEndpoint {
    cand_idx: u8,
    vid: u16,
    pid: u16,
    proto: GenericProto,
}

impl GenericEndpoint {
    const fn empty() -> Self {
        Self {
            cand_idx: 0,
            vid: 0,
            pid: 0,
            proto: GenericProto::None,
        }
    }
}

/// Stateful, allocation-free multi-driver HID + Bluetooth scanner.
///
/// Large enumeration arrays live directly inside `HidScanner` so static `.bss`
/// storage avoids stack overflow (`___chkstk_ms`) under `-nostdlib`.
pub struct HidScanner {
    io_event: HANDLE,
    candidates_cached: bool,
    cand_count: u8,
    logi_ep_count: u8,
    gen_ep_count: u8,
    present_vp_len: u8,
    present_vp: [u32; MAX_PRESENT_VP],
    logi_eps: [LogitechEndpoint; LOGI_EP_CAP],
    gen_eps: [GenericEndpoint; GEN_EP_CAP],
    candidates: [RawCandidate; CAND_CAP],
    raw_list: [RAWINPUTDEVICELIST; RAW_LIST_CAP],
    bt_id_buf: [u16; BT_ID_BUF_CAP],
}

impl Default for HidScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HidScanner {
    fn drop(&mut self) {
        self.close();
    }
}

impl HidScanner {
    pub fn new() -> Self {
        Self {
            io_event: null_mut(),
            candidates_cached: false,
            cand_count: 0,
            logi_ep_count: 0,
            gen_ep_count: 0,
            present_vp_len: 0,
            present_vp: [0; MAX_PRESENT_VP],
            logi_eps: [LogitechEndpoint::empty(); LOGI_EP_CAP],
            gen_eps: [GenericEndpoint::empty(); GEN_EP_CAP],
            candidates: [RawCandidate::empty(); CAND_CAP],
            raw_list: [RAWINPUTDEVICELIST::default(); RAW_LIST_CAP],
            bt_id_buf: [0; BT_ID_BUF_CAP],
        }
    }

    /// Initialize `HidScanner` directly in zero-initialized `.bss` storage without stack temporaries.
    ///
    /// # Safety
    /// `dst` must point to valid zero-initialized memory for `Self`.
    pub unsafe fn init_in_place(dst: *mut Self) {
        addr_of_mut!((*dst).io_event).write(null_mut());
        addr_of_mut!((*dst).candidates_cached).write(false);
        addr_of_mut!((*dst).cand_count).write(0);
        addr_of_mut!((*dst).logi_ep_count).write(0);
        addr_of_mut!((*dst).gen_ep_count).write(0);
        addr_of_mut!((*dst).present_vp_len).write(0);
        for i in 0..LOGI_EP_CAP {
            addr_of_mut!((*dst).logi_eps[i]).write(LogitechEndpoint::empty());
        }
        for i in 0..GEN_EP_CAP {
            addr_of_mut!((*dst).gen_eps[i]).write(GenericEndpoint::empty());
        }
    }

    /// Release cached `io_event` handle cleanly in place without moving `Self`.
    pub fn close(&mut self) {
        if !self.io_event.is_null() {
            unsafe {
                let _ = CloseHandle(self.io_event);
            }
            self.io_event = null_mut();
        }
    }

    unsafe fn ensure_event(&mut self) -> HANDLE {
        if self.io_event.is_null() {
            self.io_event = CreateEventW(null(), 1, 0, null());
        }
        self.io_event
    }

    fn record_present_vp(&mut self, vid: u16, pid: u16) {
        let vp = vp_u32(vid, pid);
        let n = self.present_vp_len as usize;
        if self.present_vp[..n].contains(&vp) {
            return;
        }
        if n < MAX_PRESENT_VP {
            self.present_vp[n] = vp;
            self.present_vp_len += 1;
        }
    }

    #[inline(never)]
    unsafe fn refresh_raw_candidates(&mut self) {
        self.cand_count = 0;
        self.present_vp_len = 0;

        let elem_size = core::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
        let mut cap = self.raw_list.len() as u32;
        let got = GetRawInputDeviceList(self.raw_list.as_mut_ptr(), &mut cap, elem_size);
        if got == u32::MAX {
            return;
        }
        self.candidates_cached = true;

        let count = (got as usize).min(self.raw_list.len());
        for i in 0..count {
            let item = self.raw_list[i];
            if item.dw_type != RIM_TYPEHID {
                continue;
            }
            let mut info = RID_DEVICE_INFO::default();
            let mut info_sz = core::mem::size_of::<RID_DEVICE_INFO>() as u32;
            if GetRawInputDeviceInfoW(
                item.h_device,
                RIDI_DEVICEINFO,
                &mut info as *mut RID_DEVICE_INFO as *mut c_void,
                &mut info_sz,
            ) == u32::MAX
            {
                continue;
            }
            let h = info.u.hid;
            let vid = h.dw_vendor_id as u16;
            let pid = h.dw_product_id as u16;
            let up = h.us_usage_page;
            let u = h.us_usage;

            let matched = logitech::is_candidate(vid, pid, up, u)
                || hyperx::is_candidate(vid, pid, up)
                || razer::is_candidate(vid, pid, up, u)
                || steelseries::is_candidate(vid, pid, up, u)
                || corsair::is_candidate(vid, pid, up, u)
                || pulsar::is_candidate(vid, pid, up, u);
            if !matched {
                continue;
            }

            self.record_present_vp(vid, pid);

            if (self.cand_count as usize) >= CAND_CAP {
                continue;
            }
            let dst_idx = self.cand_count as usize;
            self.candidates[dst_idx] = RawCandidate::empty();
            let mut char_len = self.candidates[dst_idx].path.buf.len() as u32;
            let res = GetRawInputDeviceInfoW(
                item.h_device,
                RIDI_DEVICENAME,
                self.candidates[dst_idx].path.buf.as_mut_ptr() as *mut c_void,
                &mut char_len,
            );
            if res == u32::MAX || res == 0 {
                continue;
            }
            self.candidates[dst_idx].path.buf[self.candidates[dst_idx].path.buf.len() - 1] = 0;
            self.candidates[dst_idx].vid = vid;
            self.candidates[dst_idx].pid = pid;
            self.candidates[dst_idx].usage_page = up;
            self.candidates[dst_idx].usage = u;
            self.cand_count += 1;
        }

        // Prioritize single-device LIGHTSPEED receivers, then vendor usage pages (>= 0xFF00)
        let n = self.cand_count as usize;
        let rank = |c: &RawCandidate| -> u8 {
            if c.vid == 0x046D && logitech::is_single_receiver_pid(c.pid) && c.usage_page == 0xFF00 && c.usage == 0x0002 {
                0
            } else if c.usage_page >= 0xFF00 {
                1
            } else {
                2
            }
        };
        for i in 1..n {
            let mut j = i;
            while j > 0 && rank(&self.candidates[j]) < rank(&self.candidates[j - 1]) {
                self.candidates.swap(j - 1, j);
                j -= 1;
            }
        }
    }

    unsafe fn poll_one_generic(
        ev: HANDLE,
        path_ptr: *const u16,
        vid: u16,
        pid: u16,
        proto: GenericProto,
    ) -> (bool, Option<Battery>) {
        let h_opt = match proto {
            GenericProto::Razer(_, _) => open_hid_feature(path_ptr),
            _ => open_hid(path_ptr),
        };
        let Some(h) = h_opt else {
            return (false, None);
        };
        let res = match proto {
            GenericProto::Hyperx(hp) => hyperx::poll_handle(h, ev, vid, pid, hp),
            GenericProto::Razer(k, tx) => razer::poll_handle(h, ev, pid, k, tx),
            GenericProto::Steel(sp) => steelseries::poll_handle(h, ev, pid, sp),
            GenericProto::Corsair(cp) => corsair::poll_handle(h, ev, pid, cp),
            GenericProto::Pulsar => pulsar::poll_handle(h, ev, vid, pid),
            GenericProto::None => None,
        };
        let _ = CloseHandle(h);
        (true, res)
    }

    fn has_gen_vp(&self, vid: u16, pid: u16) -> bool {
        let n = (self.gen_ep_count as usize).min(GEN_EP_CAP);
        self.gen_eps[..n]
            .iter()
            .any(|e| !matches!(e.proto, GenericProto::None) && e.vid == vid && e.pid == pid)
    }

    #[inline(never)]
    unsafe fn poll_and_discover_generic(
        &mut self,
        ev: HANDLE,
        skip_discover: bool,
        results: &mut BatteryList,
    ) {
        let cands = &self.candidates[..(self.cand_count as usize).min(CAND_CAP)];
        let count = (self.gen_ep_count as usize).min(GEN_EP_CAP);
        let mut any_open_fail = false;

        // 1. Poll cached generic endpoints
        for ep_slot in self.gen_eps.iter_mut().take(count) {
            let ep = *ep_slot;
            if matches!(ep.proto, GenericProto::None) {
                continue;
            }
            let ci = ep.cand_idx as usize;
            if ci >= cands.len() {
                ep_slot.proto = GenericProto::None;
                continue;
            }
            let (opened, reading) =
                Self::poll_one_generic(ev, cands[ci].path.buf.as_ptr(), ep.vid, ep.pid, ep.proto);
            if !opened {
                any_open_fail = true;
                ep_slot.proto = GenericProto::None;
                continue;
            }
            if let Some(b) = reading {
                results.upsert(b);
            }
        }

        // Compact dropped generic endpoints
        let mut w = 0usize;
        for r in 0..count {
            if !matches!(self.gen_eps[r].proto, GenericProto::None) {
                self.gen_eps[w] = self.gen_eps[r];
                w += 1;
            }
        }
        self.gen_ep_count = w as u8;

        if any_open_fail {
            self.candidates_cached = false;
            return;
        }
        if skip_discover {
            return;
        }

        // 2. Discover missing generic devices
        let cand_n = (self.cand_count as usize).min(CAND_CAP);
        for ci in 0..cand_n {
            if (self.gen_ep_count as usize) >= GEN_EP_CAP {
                break;
            }
            let c = self.candidates[ci];
            if c.vid == 0x046D || self.has_gen_vp(c.vid, c.pid) {
                continue;
            }
            let proto = if let Some(hp) = hyperx::match_profile(c.vid, c.pid, c.usage_page) {
                GenericProto::Hyperx(hp)
            } else if let Some((k, tx)) = razer::lookup(c.pid).filter(|_| razer::is_candidate(c.vid, c.pid, c.usage_page, c.usage)) {
                GenericProto::Razer(k, tx)
            } else if let Some(sp) = steelseries::lookup(c.pid).filter(|_| steelseries::is_candidate(c.vid, c.pid, c.usage_page, c.usage)) {
                GenericProto::Steel(sp)
            } else if let Some(cp) = corsair::match_proto(c.vid, c.pid, c.usage_page, c.usage) {
                GenericProto::Corsair(cp)
            } else if pulsar::is_candidate(c.vid, c.pid, c.usage_page, c.usage) {
                GenericProto::Pulsar
            } else {
                continue;
            };

            let (opened, reading) =
                Self::poll_one_generic(ev, c.path.buf.as_ptr(), c.vid, c.pid, proto);
            if !opened {
                self.candidates_cached = false;
                continue;
            }
            if let Some(b) = reading {
                let idx = self.gen_ep_count as usize;
                self.gen_eps[idx] = GenericEndpoint {
                    cand_idx: ci as u8,
                    vid: c.vid,
                    pid: c.pid,
                    proto,
                };
                self.gen_ep_count += 1;
                results.upsert(b);
            }
        }
    }

    pub fn poll(&mut self, force_rescan: bool) -> PollOutput {
        if force_rescan {
            self.candidates_cached = false;
            self.cand_count = 0;
            self.logi_ep_count = 0;
            self.gen_ep_count = 0;
            self.present_vp_len = 0;
        }

        let mut out = PollOutput::empty();
        let ev = unsafe { self.ensure_event() };
        if ev.is_null() {
            return out;
        }

        unsafe {
            if !self.candidates_cached {
                self.refresh_raw_candidates();
            }

            let cands = &self.candidates[..(self.cand_count as usize).min(CAND_CAP)];
            let logi_open_fail = logitech::poll_cached(
                ev,
                cands,
                &mut self.logi_eps,
                &mut self.logi_ep_count,
                &mut out.devices,
            );
            if logi_open_fail {
                self.candidates_cached = false;
            } else if force_rescan
                || self.logi_ep_count == 0
                || logitech::has_undiscovered_slots(cands, &self.logi_eps, self.logi_ep_count)
            {
                let disc_fail = logitech::discover(
                    ev,
                    cands,
                    &mut self.logi_eps,
                    &mut self.logi_ep_count,
                    &mut out.devices,
                );
                if disc_fail {
                    self.candidates_cached = false;
                }
            }

            self.poll_and_discover_generic(ev, false, &mut out.devices);

            bluetooth::poll_bluetooth(&mut self.bt_id_buf, &mut out.devices);
        }

        let vp_n = (self.present_vp_len as usize).min(MAX_PRESENT_VP);
        out.present_hid_vp[..vp_n].copy_from_slice(&self.present_vp[..vp_n]);
        out.present_hid_vp_len = vp_n;
        out
    }
}

/// Convenience one-shot scan for CLI tools and tests (not called in `main.rs`).
pub fn read_all() -> BatteryList {
    HidScanner::new().poll(true).devices
}

/// Inspect active Raw Input HID candidate paths & PIDs (for diagnostic CLI `poc`).
pub fn probe_raw_candidates() -> ([RawCandidate; CAND_CAP], usize) {
    let mut scanner = HidScanner::new();
    unsafe { scanner.refresh_raw_candidates() };
    let n = (scanner.cand_count as usize).min(CAND_CAP);
    (scanner.candidates, n)
}

/// Inspect Bluetooth PnP devices via `cfgmgr32` (for diagnostic CLI `poc`).
pub fn probe_bluetooth_diagnostics(cb: bluetooth::BtDiagCallback<'_>) {
    let mut buf = [0u16; BT_ID_BUF_CAP];
    unsafe { bluetooth::probe_bluetooth_diagnostics(&mut buf, cb) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_list_sorts_and_prefers_native_hid_over_bluetooth() {
        let mut list = BatteryList::new();
        list.upsert(Battery::with_id(200, 0x045E_0B13, Kind::Gamepad, Some(50), false, true, Source::Bluetooth));
        list.upsert(Battery::with_id(100, 0x1532_0290, Kind::Keyboard, Some(60), false, true, Source::Bluetooth));
        list.upsert(Battery::with_id(10, 0x1532_0290, Kind::Keyboard, Some(64), true, true, Source::NativeHid));
        list.upsert(Battery::with_id(1, 0x046D_C539, Kind::Mouse, Some(78), false, true, Source::NativeHid));
        // Bluetooth duplicate of Keyboard (1532:0290) arriving later must NOT overwrite NativeHid
        list.upsert(Battery::with_id(101, 0x1532_0290, Kind::Keyboard, Some(55), false, true, Source::Bluetooth));

        assert_eq!(list.count(), 3);
        let items: Vec<&Battery> = list.iter().collect();
        assert_eq!(items[0].kind, Kind::Mouse);
        assert_eq!(items[1].kind, Kind::Keyboard);
        assert_eq!(items[1].source, Source::NativeHid);
        assert_eq!(items[1].pct, Some(64));
        assert_eq!(items[2].kind, Kind::Gamepad);
    }

    #[test]
    fn voltage_curve_boundaries() {
        assert_eq!(voltage_to_pct(4200), 100);
        assert_eq!(voltage_to_pct(4186), 100);
        assert_eq!(voltage_to_pct(4156), 99);
        assert_eq!(voltage_to_pct(3815), 51);
        assert_eq!(voltage_to_pct(3485), 1);
        assert_eq!(voltage_to_pct(3400), 0);
    }

    #[test]
    fn hid_path_is_4_byte_aligned_for_raw_input() {
        assert!(core::mem::align_of::<HidPath>() >= 4);

        let mut pair = core::hint::black_box((1u16, HidPath::empty()));
        assert_eq!(pair.0, 1);
        let addr = pair.1.buf.as_ptr() as usize;
        assert_eq!(addr & 3, 0, "HidPath must be 4-byte aligned, got {addr:#x}");

        unsafe {
            let elem_size = core::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
            let mut list = [RAWINPUTDEVICELIST::default(); RAW_LIST_CAP];
            let mut cap = list.len() as u32;
            let got = GetRawInputDeviceList(list.as_mut_ptr(), &mut cap, elem_size);
            if got != u32::MAX {
                if let Some(item) = list[..got as usize]
                    .iter()
                    .find(|d| d.dw_type == RIM_TYPEHID)
                {
                    let mut char_len = pair.1.buf.len() as u32;
                    let res = GetRawInputDeviceInfoW(
                        item.h_device,
                        RIDI_DEVICENAME,
                        pair.1.buf.as_mut_ptr() as *mut c_void,
                        &mut char_len,
                    );
                    assert_ne!(
                        res,
                        u32::MAX,
                        "RIDI_DEVICENAME failed (err={})",
                        crate::win32::GetLastError()
                    );
                    assert!(res > 0);
                }
            }
        }
    }
}
