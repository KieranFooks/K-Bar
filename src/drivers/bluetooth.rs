//! Generic Windows Bluetooth battery reader via `cfgmgr32.dll` (`DEVPKEY_Bluetooth_Battery`).
//!
//! Covers BLE GATT Battery Service (`0x180F`) and Bluetooth Classic HFP devices
//! (keyboards, mice, headsets/earbuds, Xbox and other Bluetooth gamepads) with zero WinRT/COM.

use core::ptr::null;

use crate::hid::{Battery, BatteryList, Kind, Source};
use crate::w;
use crate::win32::{
    CM_Get_DevNode_PropertyW, CM_Get_Device_ID_ListW, CM_Get_Device_ID_List_SizeW,
    CM_Get_Parent, CM_Locate_DevNodeW, CM_GETIDLIST_FILTER_ENUMERATOR,
    CM_GETIDLIST_FILTER_PRESENT, CM_LOCATE_DEVNODE_NORMAL, CM_LOCATE_DEVNODE_PHANTOM, CR_SUCCESS,
    DEVPROPKEY, DEVPROP_TYPE_BOOLEAN, DEVPROP_TYPE_BYTE, DEVPROP_TYPE_STRING,
    DEVPROP_TYPE_STRING_LIST, DEVPROP_TYPE_UINT16, DEVPROP_TYPE_UINT32, GUID,
};

const BT_ID_BUF_CAP: usize = 8192;

pub type BtDiagCallback<'a> = &'a mut dyn FnMut(&[u16], Option<u8>, Option<bool>, Option<Kind>, &[u16]);

const DEVPKEY_BLUETOOTH_BATTERY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID {
        data1: 0x104E_A319,
        data2: 0x6EE2,
        data3: 0x4701,
        data4: [0xBD, 0x47, 0x8D, 0xDB, 0xF4, 0x25, 0xBB, 0xE5],
    },
    pid: 2,
};

const DEVPKEY_DEVICE_IS_CONNECTED: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID {
        data1: 0x83DA_6326,
        data2: 0x97A6,
        data3: 0x4088,
        data4: [0x94, 0x53, 0xA1, 0x92, 0x3F, 0x57, 0x3B, 0x29],
    },
    pid: 15,
};

const DEVPKEY_DEVICECONTAINER_CATEGORY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID {
        data1: 0x78C3_4FC8,
        data2: 0x104A,
        data3: 0x4ACA,
        data4: [0x9E, 0xA4, 0x52, 0x4D, 0x52, 0x99, 0x6E, 0x57],
    },
    pid: 90,
};

const DEVPKEY_BLUETOOTH_DEVICE_ADDRESS: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID {
        data1: 0x2BD6_7D8B,
        data2: 0x8BEB,
        data3: 0x48D5,
        data4: [0x87, 0xE0, 0x6C, 0xDA, 0x34, 0x28, 0x04, 0x0A],
    },
    pid: 1,
};

const DEVPKEY_BLUETOOTH_DEVICE_VID: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID {
        data1: 0x2BD6_7D8B,
        data2: 0x8BEB,
        data3: 0x48D5,
        data4: [0x87, 0xE0, 0x6C, 0xDA, 0x34, 0x28, 0x04, 0x0A],
    },
    pid: 7,
};

const DEVPKEY_BLUETOOTH_DEVICE_PID: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID {
        data1: 0x2BD6_7D8B,
        data2: 0x8BEB,
        data3: 0x48D5,
        data4: [0x87, 0xE0, 0x6C, 0xDA, 0x34, 0x28, 0x04, 0x0A],
    },
    pid: 8,
};

const DEVPKEY_BLUETOOTH_CLASS_OF_DEVICE: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID {
        data1: 0x2BD6_7D8B,
        data2: 0x8BEB,
        data3: 0x48D5,
        data4: [0x87, 0xE0, 0x6C, 0xDA, 0x34, 0x28, 0x04, 0x0A],
    },
    pid: 10,
};

const DEVPKEY_DEVICE_FRIENDLY_NAME: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID {
        data1: 0xA45C_254E,
        data2: 0xDF1C,
        data3: 0x4EFD,
        data4: [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0],
    },
    pid: 14,
};

fn ascii_lower(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b + 32
    } else {
        b
    }
}

/// Case-insensitive ASCII substring search across a UTF-16 slice (works across `\0`-delimited STRING_LISTs).
pub fn contains_ascii_ci(haystack: &[u16], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    let limit = haystack.len() - needle.len();
    let mut i = 0;
    while i <= limit {
        let mut matched = true;
        let mut j = 0;
        while j < needle.len() {
            let u = haystack[i + j];
            if u > 0x7F || ascii_lower(u as u8) != ascii_lower(needle[j]) {
                matched = false;
                break;
            }
            j += 1;
        }
        if matched {
            return true;
        }
        i += 1;
    }
    false
}

fn contains_any(haystack: &[u16], needles: &[&[u8]]) -> bool {
    needles.iter().any(|n| contains_ascii_ci(haystack, n))
}

/// Classify a Bluetooth device into Mouse / Keyboard / Headset / Gamepad,
/// explicitly excluding phones, computers, watches, cameras, printers, and displays.
pub fn classify_bt_device(category_utf16: &[u16], cod: Option<u32>, name_utf16: &[u16]) -> Option<Kind> {
    // 1. Reject non-peripheral device classes first.
    let is_headphone_cat = contains_any(category_utf16, &[b"headphone", b"earphone"]);
    if (!is_headphone_cat && contains_ascii_ci(category_utf16, b"phone"))
        || contains_any(
            category_utf16,
            &[
                b"computer",
                b"portabledevice",
                b"watch",
                b"camera",
                b"printer",
                b"display",
                b"speaker",
            ],
        )
    {
        return None;
    }
    if let Some(c) = cod {
        let major = (c >> 8) & 0x1F;
        let minor = (c >> 2) & 0x3F;
        if major == 1 || major == 2 || (major == 4 && matches!(minor, 0x05 | 0x08 | 0x09 | 0x0A)) {
            return None;
        }
    }

    // 2. Match Windows DeviceContainer category strings (specific types before generic "gaming").
    if contains_any(
        category_utf16,
        &[b"mouse", b"trackball", b"touchpad", b"pointing"],
    ) {
        return Some(Kind::Mouse);
    }
    if contains_any(category_utf16, &[b"keyboard", b"keypad"]) {
        return Some(Kind::Keyboard);
    }
    if contains_any(
        category_utf16,
        &[b"headset", b"headphone", b"earbud", b"audio.head"],
    ) {
        return Some(Kind::Headset);
    }
    if contains_any(
        category_utf16,
        &[b"gamepad", b"joystick", b"controller", b"gaming"],
    ) {
        return Some(Kind::Gamepad);
    }

    // 3. Reject speakers by name when no explicit peripheral category matched, then match name.
    if contains_any(name_utf16, &[b"speaker", b"soundlink"]) {
        return None;
    }
    let name_kind = classify_by_name(name_utf16);
    if name_kind.is_some() {
        return name_kind;
    }

    // 4. Match Bluetooth Class of Device (CoD).
    if let Some(c) = cod {
        let major = (c >> 8) & 0x1F;
        let minor = (c >> 2) & 0x3F;
        if major == 0x05 {
            if matches!(minor & 0x0F, 1 | 2) {
                return Some(Kind::Gamepad);
            }
            if (minor & 0x10) != 0 {
                return Some(Kind::Keyboard);
            }
            if (minor & 0x20) != 0 {
                return Some(Kind::Mouse);
            }
        } else if major == 0x04 && matches!(minor, 0x01 | 0x02 | 0x06 | 0x07) {
            return Some(Kind::Headset);
        }
    }

    None
}

fn classify_by_name(name: &[u16]) -> Option<Kind> {
    if contains_any(
        name,
        &[
            b"xbox",
            b"controller",
            b"gamepad",
            b"dualsense",
            b"dualshock",
            b"joy-con",
            b"8bitdo",
            b"stadia",
        ],
    ) {
        return Some(Kind::Gamepad);
    }
    if contains_any(
        name,
        &[
            b"keyboard",
            b"clavier",
            b"kbd",
            b"keychron",
            b"nuphy",
            b"mx keys",
            b"k380",
            b"k400",
            b"k780",
            b"g915",
            b"blackwidow",
            b"deathstalker",
            b"lofree",
            b"hhkb",
            b"anne pro",
            b"rk61",
            b"rk84",
        ],
    ) {
        return Some(Kind::Keyboard);
    }
    if contains_any(
        name,
        &[
            b"mouse",
            b"souris",
            b"mx master",
            b"mx anywhere",
            b"mx vertical",
            b"mx ergo",
            b"g502",
            b"g305",
            b"superlight",
            b"viper",
            b"deathadder",
            b"basilisk",
            b"orochi",
            b"aerox",
        ],
    ) {
        return Some(Kind::Mouse);
    }
    if contains_any(
        name,
        &[
            b"headset",
            b"headphone",
            b"casque",
            b"earbud",
            b"airpods",
            b"buds",
            b"arctis",
            b"cloud",
            b"wh-1000",
            b"wf-1000",
            b"quietcomfort",
            b"bose",
            b"jabra",
            b"momentum",
            b"virtuoso",
            b"hs80",
            b"g733",
            b"g535",
            b"g435",
            b"barracuda",
            b"blackshark",
            b"pulse 3d",
            b"inzone",
            b"maxwell",
        ],
    ) {
        return Some(Kind::Headset);
    }
    None
}

/// Parse 12 hex characters (e.g. `441622b63a70`) into a 48-bit MAC address.
pub fn parse_bt_mac_utf16(wide: &[u16]) -> Option<u64> {
    let mut acc = 0u64;
    let mut digits = 0u8;
    for &u in wide {
        if u == 0 {
            break;
        }
        let d = match u as u8 {
            b'0'..=b'9' if u <= 0x7F => (u as u8 - b'0') as u64,
            b'a'..=b'f' if u <= 0x7F => (u as u8 - b'a' + 10) as u64,
            b'A'..=b'F' if u <= 0x7F => (u as u8 - b'A' + 10) as u64,
            b':' | b'-' => continue,
            _ => return None,
        };
        acc = (acc << 4) | d;
        digits += 1;
        if digits == 12 {
            return Some(acc);
        }
    }
    None
}

fn fnv1a_utf16(wide: &[u16]) -> u64 {
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    for &u in wide {
        if u == 0 {
            break;
        }
        h ^= (u & 0xFF) as u64;
        h = h.wrapping_mul(0x0100_0000_01B3);
        h ^= (u >> 8) as u64;
        h = h.wrapping_mul(0x0100_0000_01B3);
    }
    h
}

unsafe fn read_devnode_prop(
    dn: u32,
    key: &DEVPROPKEY,
    buf: &mut [u8; 256],
) -> Option<(u32, usize)> {
    let mut ptype = 0u32;
    let mut sz = buf.len() as u32;
    if CM_Get_DevNode_PropertyW(dn, key, &mut ptype, buf.as_mut_ptr(), &mut sz, 0) == CR_SUCCESS {
        Some((ptype, (sz as usize).min(buf.len())))
    } else {
        None
    }
}

unsafe fn copy_utf16_prop(buf: &[u8], len_bytes: usize, dst: &mut [u16; 128]) -> usize {
    let units = (len_bytes / 2).min(dst.len());
    let mut i = 0;
    while i < units {
        dst[i] = u16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]);
        i += 1;
    }
    units
}

#[inline(never)]
unsafe fn inspect_bt_devnode(
    id_ptr: *const u16,
    id_slice: &[u16],
    phantom: bool,
    results: &mut BatteryList,
    mut diag: Option<BtDiagCallback<'_>>,
) {
    let mut dn = 0u32;
    let flags = if phantom {
        CM_LOCATE_DEVNODE_PHANTOM
    } else {
        CM_LOCATE_DEVNODE_NORMAL
    };
    if CM_Locate_DevNodeW(&mut dn, id_ptr, flags) != CR_SUCCESS {
        return;
    }

    let mut prop_buf = [0u8; 256];
    let batt = read_devnode_prop(dn, &DEVPKEY_BLUETOOTH_BATTERY, &mut prop_buf)
        .and_then(|(ty, n)| (ty == DEVPROP_TYPE_BYTE && n >= 1 && prop_buf[0] <= 100).then_some(prop_buf[0]));

    if batt.is_none() && diag.is_none() {
        return;
    }

    let mut connected: Option<bool> = None;
    let mut cat_buf = [0u16; 128];
    let mut cat_len = 0usize;
    let mut name_buf = [0u16; 128];
    let mut name_len = 0usize;
    let mut cod: Option<u32> = None;
    let mut mac: Option<u64> = None;
    let mut vid: u16 = 0;
    let mut pid: u16 = 0;

    let mut cur = dn;
    for depth in 0..4 {
        if connected.is_none() {
            if let Some((ty, n)) = read_devnode_prop(cur, &DEVPKEY_DEVICE_IS_CONNECTED, &mut prop_buf) {
                if ty == DEVPROP_TYPE_BOOLEAN && n >= 1 {
                    connected = Some(prop_buf[0] != 0);
                }
            }
        }
        if cat_len == 0 {
            if let Some((ty, n)) = read_devnode_prop(cur, &DEVPKEY_DEVICECONTAINER_CATEGORY, &mut prop_buf) {
                if (ty == DEVPROP_TYPE_STRING_LIST || ty == DEVPROP_TYPE_STRING) && n >= 2 {
                    cat_len = copy_utf16_prop(&prop_buf, n, &mut cat_buf);
                }
            }
        }
        if name_len == 0 {
            if let Some((ty, n)) = read_devnode_prop(cur, &DEVPKEY_DEVICE_FRIENDLY_NAME, &mut prop_buf) {
                if ty == DEVPROP_TYPE_STRING && n >= 2 {
                    name_len = copy_utf16_prop(&prop_buf, n, &mut name_buf);
                }
            }
        }
        if cod.is_none() {
            if let Some((ty, n)) = read_devnode_prop(cur, &DEVPKEY_BLUETOOTH_CLASS_OF_DEVICE, &mut prop_buf) {
                if ty == DEVPROP_TYPE_UINT32 && n >= 4 {
                    cod = Some(u32::from_le_bytes([
                        prop_buf[0],
                        prop_buf[1],
                        prop_buf[2],
                        prop_buf[3],
                    ]));
                }
            }
        }
        if mac.is_none() {
            if let Some((ty, n)) = read_devnode_prop(cur, &DEVPKEY_BLUETOOTH_DEVICE_ADDRESS, &mut prop_buf) {
                if ty == DEVPROP_TYPE_STRING && n >= 24 {
                    let mut tmp = [0u16; 128];
                    let tlen = copy_utf16_prop(&prop_buf, n, &mut tmp);
                    mac = parse_bt_mac_utf16(&tmp[..tlen]);
                }
            }
        }
        if vid == 0 {
            if let Some((ty, n)) = read_devnode_prop(cur, &DEVPKEY_BLUETOOTH_DEVICE_VID, &mut prop_buf) {
                if ty == DEVPROP_TYPE_UINT16 && n >= 2 {
                    vid = u16::from_le_bytes([prop_buf[0], prop_buf[1]]);
                }
            }
        }
        if pid == 0 {
            if let Some((ty, n)) = read_devnode_prop(cur, &DEVPKEY_BLUETOOTH_DEVICE_PID, &mut prop_buf) {
                if ty == DEVPROP_TYPE_UINT16 && n >= 2 {
                    pid = u16::from_le_bytes([prop_buf[0], prop_buf[1]]);
                }
            }
        }

        if depth == 3 {
            break;
        }
        let mut parent = 0u32;
        if CM_Get_Parent(&mut parent, cur, 0) != CR_SUCCESS || parent == 0 || parent == cur {
            break;
        }
        cur = parent;
    }

    let kind = classify_bt_device(&cat_buf[..cat_len], cod, &name_buf[..name_len]);

    if let Some(ref mut cb) = diag {
        let name_slice = {
            let n = name_buf[..name_len]
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(name_len);
            &name_buf[..n]
        };
        cb(id_slice, batt, connected, kind, name_slice);
    }

    let Some(pct) = batt else { return };
    if connected != Some(true) {
        return;
    }
    let Some(kind) = kind else { return };

    let addr_key = mac.unwrap_or_else(|| fnv1a_utf16(id_slice) & 0x00FF_FFFF_FFFF_FFFF);
    let id = (2u64 << 56) | (addr_key & 0x00FF_FFFF_FFFF_FFFF);
    let vid_pid = if vid != 0 || pid != 0 {
        ((vid as u32) << 16) | (pid as u32)
    } else {
        0
    };

    results.upsert(Battery::with_id(
        id,
        vid_pid,
        kind,
        Some(pct),
        false,
        true,
        Source::Bluetooth,
    ));
}

#[inline(never)]
unsafe fn scan_enumerators(
    id_buf: &mut [u16; BT_ID_BUF_CAP],
    present_only: bool,
    results: &mut BatteryList,
    mut diag: Option<BtDiagCallback<'_>>,
) {
    let enums = [
        w!("BTHLE"),
        w!("BTHENUM"),
        w!("BTHHFENUM"),
        w!("BTHLEDEVICE"),
    ];
    let flags = if present_only {
        CM_GETIDLIST_FILTER_ENUMERATOR | CM_GETIDLIST_FILTER_PRESENT
    } else {
        CM_GETIDLIST_FILTER_ENUMERATOR
    };

    for &enumerator in &enums {
        let mut req_len = 0u32;
        if CM_Get_Device_ID_List_SizeW(&mut req_len, enumerator, flags) != CR_SUCCESS || req_len <= 1 {
            continue;
        }
        if (req_len as usize) > id_buf.len() {
            continue;
        }
        if CM_Get_Device_ID_ListW(enumerator, id_buf.as_mut_ptr(), id_buf.len() as u32, flags)
            != CR_SUCCESS
        {
            continue;
        }
        id_buf[id_buf.len() - 1] = 0;

        let used = (req_len as usize).min(id_buf.len());
        let mut pos = 0usize;
        while pos < used && id_buf[pos] != 0 {
            let start = pos;
            while pos < used && id_buf[pos] != 0 {
                pos += 1;
            }
            if pos > start && pos < used {
                let id_ptr = id_buf[start..].as_ptr();
                let id_slice = &id_buf[start..pos];
                let cb_ref = diag.as_mut().map(|f| &mut **f as BtDiagCallback<'_>);
                inspect_bt_devnode(id_ptr, id_slice, !present_only, results, cb_ref);
            }
            pos += 1;
        }
    }
    let _ = null::<u16>();
}

/// Poll connected Bluetooth devices that expose `DEVPKEY_Bluetooth_Battery`.
///
/// # Safety
/// Must be called with valid access to a caller-owned `.bss` `id_buf`.
#[inline(never)]
pub unsafe fn poll_bluetooth(id_buf: &mut [u16; BT_ID_BUF_CAP], results: &mut BatteryList) {
    scan_enumerators(id_buf, true, results, None);
}

/// Diagnostic enumeration for `poc.rs`, including paired-offline Bluetooth entries.
///
/// # Safety
/// Caller must pass a valid scratch buffer and callback.
pub unsafe fn probe_bluetooth_diagnostics(
    id_buf: &mut [u16; BT_ID_BUF_CAP],
    cb: BtDiagCallback<'_>,
) {
    let mut dummy = BatteryList::new();
    scan_enumerators(id_buf, false, &mut dummy, Some(cb));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_str(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn classifies_xbox_gamepad_keyboard_mouse_headset_and_rejects_phones() {
        assert_eq!(
            classify_bt_device(&u16_str("Input.Gaming.Gamepad"), None, &u16_str("Xbox Wireless Controller")),
            Some(Kind::Gamepad)
        );
        assert_eq!(
            classify_bt_device(&u16_str("Input.Keyboard"), None, &u16_str("Keychron K3 Pro")),
            Some(Kind::Keyboard)
        );
        assert_eq!(
            classify_bt_device(&u16_str("Input.Mouse"), None, &u16_str("MX Master 3S")),
            Some(Kind::Mouse)
        );
        assert_eq!(
            classify_bt_device(&u16_str("Audio.Headphones"), None, &u16_str("WH-1000XM5")),
            Some(Kind::Headset)
        );
        // Phone rejected despite friendly name
        assert_eq!(
            classify_bt_device(&u16_str("Communication.Phone"), Some(0x5A020C), &u16_str("Galaxy S25")),
            None
        );
        // CoD fallback: Peripheral Keyboard (major=5, minor=0x10 -> 0x0540)
        assert_eq!(
            classify_bt_device(&[], Some(0x000540), &u16_str("Bluetooth HID Device")),
            Some(Kind::Keyboard)
        );
        // CoD fallback: Peripheral Mouse (major=5, minor=0x20 -> 0x0580)
        assert_eq!(
            classify_bt_device(&[], Some(0x000580), &u16_str("BT5.1 Device")),
            Some(Kind::Mouse)
        );
        // CoD fallback: Audio Headset (major=4, minor=0x01 -> 0x0404)
        assert_eq!(
            classify_bt_device(&[], Some(0x240404), &u16_str("BT Audio")),
            Some(Kind::Headset)
        );
        // Gaming.Mouse and Gaming.Headset must not be shadowed by "gaming"
        assert_eq!(
            classify_bt_device(&u16_str("Input.Gaming.Mouse"), None, &u16_str("G309")),
            Some(Kind::Mouse)
        );
        assert_eq!(
            classify_bt_device(&u16_str("Audio.Gaming.Headset"), None, &u16_str("Nova 7")),
            Some(Kind::Headset)
        );
        // Speakers excluded even for headset brands
        assert_eq!(
            classify_bt_device(&[], Some(0x240414), &u16_str("Bose SoundLink")),
            None
        );
    }

    #[test]
    fn parses_bt_mac_addresses() {
        assert_eq!(parse_bt_mac_utf16(&u16_str("441622b63a70")), Some(0x4416_22B6_3A70));
        assert_eq!(parse_bt_mac_utf16(&u16_str("44:16:22:B6:3A:70")), Some(0x4416_22B6_3A70));
        assert_eq!(parse_bt_mac_utf16(&u16_str("short")), None);
    }
}
