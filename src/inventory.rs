//! Peripheral inventory: the slot cards and drives the power budget charges, and the peripheral temperature
//! sensors that steer a zone fan. Everything is discovered from sysfs attributes the kernel caches (PCI config
//! identity, SCSI VPD pages, hwmon names); nothing here issues a command to a device.
use crate::{
    config::{Config, SensorSpec},
    hardware::{children, named_children, number, pci_chain, text, Paths},
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// A device charged to the power budget beyond the measured base.
#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    /// Stable identity for this boot: the PCI card address without its function (`0000:12:00`) or the block
    /// device name (`sdb`).
    pub key: String,
    /// The `DEVICE_W` class: mellanox, nvme, nvme-switched, pcie-switch, pcie-card, disk, ssd or usb-disk.
    pub class: String,
    /// What the kernel reports the device to be (PCI id, NVMe or SCSI model).
    pub label: String,
    pub slot: Option<String>,
    /// The fan that cools it, or None when it sits outside the governed zones (USB, Thunderbolt).
    pub zone: Option<usize>,
    /// The allowance charged to the budget (`DEVICE_PCI_W` by PCI id, else `DEVICE_W` by class).
    pub watts: f64,
}

/// A peripheral temperature sensor (nvme, drivetemp, mlx5 hwmon) and the zone fan it steers.
#[derive(Clone, Debug, PartialEq)]
pub struct PeriphSensor {
    /// The engine's sensor key: `nvme` for the Apple flash, else `<hwmon name>:<device>`.
    pub key: String,
    /// The hwmon driver name (the `PERIPH` and `PERIPH_POLL` class).
    pub class: String,
    /// The canonical sysfs device directory its hwmon hangs under (survives hwmon renumbering).
    pub dev: PathBuf,
    /// The inventory device it belongs to (None for the Apple flash).
    pub device: Option<String>,
    pub spec: SensorSpec,
    /// A `TEMP_COMMAND` sensor's argument vector (`{pci}` already replaced); None for hwmon sensors.
    pub command: Option<Vec<String>>,
}

/// Slot connector addresses (`0000:11:10`) by slot number, from the firmware's slot table.
fn slots(paths: &Paths) -> BTreeMap<String, String> {
    children(paths.sys("bus/pci/slots"))
        .into_iter()
        .filter_map(|p| {
            let slot = p.file_name()?.to_str()?.split('-').next()?.to_string();
            Some((text(p.join("address"))?, slot))
        })
        .collect()
}

/// The peripheral slot a PCI device sits in: the nearest ancestor port (never the device itself) that is a slot
/// listed in `SLOT_ZONE`. Returns the slot and whether the device's parent is that port (it sits directly in the
/// slot, rather than behind a switch on the card).
fn peripheral_slot(
    c: &Config,
    slots: &BTreeMap<String, String>,
    dev: &Path,
) -> Option<(String, bool)> {
    let chain = pci_chain(dev);
    let (_, ancestors) = chain.split_last()?;
    let pos = ancestors.iter().rposition(|a| {
        slots
            .get(&a[..a.len() - 2])
            .is_some_and(|s| c.slot_zone.contains_key(s))
    })?;
    let a = &ancestors[pos];
    Some((slots[&a[..a.len() - 2]].clone(), pos + 1 == ancestors.len()))
}

fn pci_id(dev: &Path) -> Option<String> {
    let v = text(dev.join("vendor"))?;
    let d = text(dev.join("device"))?;
    Some(format!(
        "{}:{}",
        v.trim_start_matches("0x"),
        d.trim_start_matches("0x")
    ))
}

fn allowance(c: &Config, class: &str, id: Option<&str>) -> f64 {
    id.and_then(|i| c.device_pci_w.get(i).copied())
        .or_else(|| c.device_w.get(class).copied())
        .unwrap_or(0.0)
}

/// Every PCI endpoint (function 0, no virtual functions) and card switch in a `SLOT_ZONE` slot. AMD devices are
/// the MPX GPU dies' territory and are never peripherals.
fn pci_devices(paths: &Paths, c: &Config) -> Vec<Device> {
    let slots = slots(paths);
    let mut out = Vec::new();
    for d in children(paths.sys("bus/pci/devices")) {
        let Some(name) = d.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        if !name.ends_with(".0") || d.join("physfn").exists() {
            continue;
        }
        let Some((slot, direct)) = peripheral_slot(c, &slots, &d) else {
            continue;
        };
        let vendor = text(d.join("vendor")).unwrap_or_default();
        let class_code = text(d.join("class")).unwrap_or_default();
        if vendor == "0x1002" {
            continue;
        }
        let class = if class_code.starts_with("0x0604") {
            if !direct {
                continue; // a switch's downstream ports: the switch is charged once, at its upstream port
            }
            "pcie-switch"
        } else if vendor == "0x15b3" {
            "mellanox"
        } else if class_code.starts_with("0x0108") {
            if direct {
                "nvme"
            } else {
                "nvme-switched"
            }
        } else {
            "pcie-card"
        };
        let id = pci_id(&d);
        let model = named_children(d.join("nvme"), "nvme")
            .first()
            .and_then(|n| text(n.join("model")));
        let label = match (&id, model) {
            (Some(i), Some(m)) => format!("{i} {m}"),
            (Some(i), None) => i.clone(),
            (None, m) => m.unwrap_or_default(),
        };
        out.push(Device {
            key: name[..name.len() - 2].to_string(),
            class: class.into(),
            label,
            zone: c.slot_zone.get(&slot).copied(),
            slot: Some(slot),
            watts: allowance(c, class, id.as_deref()),
        });
    }
    out
}

/// The rotation rate a SCSI/ATA device reports in its Block Device Characteristics VPD page (B1), as cached by
/// the kernel: Some(1) = solid state, Some(rpm) for a spinning disk, None when not reported.
pub fn rotation_rate(scsi_dev: &Path) -> Option<u16> {
    let b = fs::read(scsi_dev.join("vpd_pgb1")).ok()?;
    if b.len() < 8 || b[1] != 0xb1 {
        return None;
    }
    match u16::from_be_bytes([b[4], b[5]]) {
        0 => None,
        r => Some(r),
    }
}

fn is_usb(dev: &Path) -> bool {
    fs::canonicalize(dev)
        .unwrap_or_else(|_| dev.to_path_buf())
        .components()
        .any(|c| c.as_os_str().to_str().is_some_and(|s| s.starts_with("usb")))
}

/// Every SCSI disk (SATA through the PCH, SAS, USB). The class comes from the drive's own rotation rate; the
/// kernel's `rotational` flag is used only when the drive reports none, and never for USB-attached disks
/// (USB storage is marked rotational whatever it is).
fn block_devices(paths: &Paths, c: &Config) -> Vec<Device> {
    let mut out = Vec::new();
    for b in named_children(paths.sys("block"), "sd") {
        let Some(name) = b.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        let dev = b.join("device");
        let usb = is_usb(&dev);
        let class = match rotation_rate(&dev) {
            Some(r) if r >= 0x401 => "disk",
            Some(1) => "ssd",
            _ if usb => "usb-disk",
            _ if text(b.join("queue/rotational")).as_deref() == Some("1") => "disk",
            _ => "ssd",
        };
        let label = [text(dev.join("vendor")), text(dev.join("model"))]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        out.push(Device {
            key: name,
            class: class.into(),
            label,
            slot: None,
            zone: if usb { None } else { Some(c.disk_zone) },
            watts: allowance(c, class, None),
        });
    }
    out
}

/// The peripheral inventory: slot cards first, then disks.
pub fn devices(paths: &Paths, c: &Config) -> Vec<Device> {
    let mut v = pci_devices(paths, c);
    v.extend(block_devices(paths, c));
    v
}

/// Device count per class, for the start-up log line.
pub fn counts(devices: &[Device]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for d in devices {
        *m.entry(d.class.clone()).or_insert(0) += 1;
    }
    m
}

/// The temperature input of a device's hwmon with the given driver name: the hwmon directory is either a child of
/// the device (`nvme/nvme1/hwmon4`) or inside its `hwmon` class directory (`3:0:0:0/hwmon/hwmon14`).
pub fn hwmon_input(dev: &Path, name: &str) -> Option<PathBuf> {
    let mut candidates = named_children(dev, "hwmon");
    if let Some(i) = candidates.iter().position(|p| p.ends_with("hwmon")) {
        let inner = candidates.remove(i);
        candidates.extend(named_children(inner, "hwmon"));
    }
    candidates
        .into_iter()
        .filter(|h| text(h.join("name")).as_deref() == Some(name))
        .map(|h| h.join("temp1_input"))
        .find(|p| p.exists())
}

/// Every peripheral temperature sensor that steers a fan: the Apple flash (`NVME`), and the `PERIPH` drivers'
/// sensors on devices in `SLOT_ZONE` slots or on internal SATA disks. Sensors of devices outside the governed zones
/// (USB, Thunderbolt) are left out: the box's fans do not cool them.
pub fn sensors(paths: &Paths, c: &Config) -> Vec<PeriphSensor> {
    let slots = slots(paths);
    let mut out = Vec::new();
    for h in named_children(paths.sys("class/hwmon"), "hwmon") {
        let Some(name) = text(h.join("name")) else {
            continue;
        };
        let Some((target, band, default_crit)) = c.periph.get(&name).copied() else {
            continue;
        };
        let Ok(dev) = fs::canonicalize(h.join("device")) else {
            continue;
        };
        if !h.join("temp1_input").exists() {
            continue;
        }
        let crit = number(h.join("temp1_crit"), 1000.0)
            .filter(|v| (50.0..=130.0).contains(v))
            .unwrap_or(default_crit);
        let chain = pci_chain(&dev);
        let pci = chain.last().cloned();
        let apple = name == "nvme"
            && pci
                .as_ref()
                .and_then(|a| text(paths.sys(&format!("bus/pci/devices/{a}/vendor"))))
                .as_deref()
                == Some("0x106b");
        if apple {
            let mut spec = c.nvme.clone();
            spec.emergency = crit - 5.0;
            out.push(PeriphSensor {
                key: "nvme".into(),
                class: name,
                dev,
                device: None,
                spec,
                command: None,
            });
            continue;
        }
        let basename = dev
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let (zone, device) = if name == "drivetemp" {
            if is_usb(&dev) {
                continue;
            }
            let blk = named_children(dev.join("block"), "sd")
                .first()
                .and_then(|b| b.file_name().map(|n| n.to_string_lossy().into_owned()));
            (c.disk_zone, blk)
        } else {
            let Some(a) = pci.as_ref() else {
                continue;
            };
            let pdev = paths.sys(&format!("bus/pci/devices/{a}"));
            let Some((slot, _)) = peripheral_slot(c, &slots, &pdev) else {
                continue;
            };
            (c.slot_zone[&slot], Some(a[..a.len() - 2].to_string()))
        };
        out.push(PeriphSensor {
            key: format!("{name}:{basename}"),
            class: name,
            dev,
            device,
            spec: SensorSpec {
                fan: zone,
                target,
                band,
                emergency: crit - 5.0,
            },
            command: None,
        });
    }
    out
}

/// `TEMP_COMMAND` sensors: for every slot card of a configured class that has no hwmon sensor, the configured
/// program with `{pci}` replaced by the card's function-0 address (for example MFT's `mget_temp -d {pci}` for
/// Mellanox cards whose firmware gives the kernel no temperature). Programs that do not exist are left out.
pub fn command_sensors(
    paths: &Paths,
    c: &Config,
    devices: &[Device],
    have: &[PeriphSensor],
) -> Vec<PeriphSensor> {
    let mut out = Vec::new();
    for d in devices {
        let (Some(argv), Some(zone)) = (c.temp_command.get(&d.class), d.zone) else {
            continue;
        };
        if d.slot.is_none() || have.iter().any(|s| s.device.as_deref() == Some(&d.key)) {
            continue;
        }
        if !Path::new(&argv[0]).is_file() {
            continue;
        }
        let Some((target, band, crit)) = c.periph.get(&d.class).copied() else {
            continue;
        };
        let pci = format!("{}.0", d.key);
        let Ok(dev) = fs::canonicalize(paths.sys(&format!("bus/pci/devices/{pci}"))) else {
            continue;
        };
        out.push(PeriphSensor {
            key: format!("{}:{}", d.class, d.key),
            class: d.class.clone(),
            dev,
            device: Some(d.key.clone()),
            spec: SensorSpec {
                fan: zone,
                target,
                band,
                emergency: crit - 5.0,
            },
            command: Some(argv.iter().map(|a| a.replace("{pci}", &pci)).collect()),
        });
    }
    out
}

/// Zone floors for devices with no live temperature sensor (`UNMONITORED` by class): a fixed rpm per fan, and the
/// fans each fan must not run below (`follow[zone][fan]`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Floors {
    pub rpm: [f64; 4],
    pub follow: [[bool; 4]; 4],
}
impl Floors {
    /// The floor of fan `i` (0-based) given every fan's present speed.
    pub fn of(&self, i: usize, speeds: &[f64; 4]) -> f64 {
        (0..4)
            .filter(|&j| self.follow[i][j])
            .map(|j| speeds[j])
            .fold(self.rpm[i], f64::max)
    }
    /// For the log: the followed fans per zone, `{"3": [4]}`.
    pub fn follow_lists(&self) -> BTreeMap<String, Vec<usize>> {
        (0..4)
            .filter(|&i| self.follow[i].iter().any(|f| *f))
            .map(|i| {
                (
                    (i + 1).to_string(),
                    (0..4)
                        .filter(|&j| self.follow[i][j])
                        .map(|j| j + 1)
                        .collect(),
                )
            })
            .collect()
    }
}
pub fn unmonitored_floors(c: &Config, devices: &[Device], monitored: &[String]) -> Floors {
    let mut f = Floors::default();
    for d in devices {
        if monitored.contains(&d.key) {
            continue;
        }
        if let (Some(z), Some((rpm, follow))) = (d.zone, c.unmonitored.get(&d.class)) {
            f.rpm[z - 1] = f.rpm[z - 1].max(*rpm);
            if let Some(j) = follow {
                if *j != z {
                    f.follow[z - 1][j - 1] = true;
                }
            }
        }
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::tests::put;
    use std::os::unix::fs::symlink;

    /// A fixture tree shaped like the 2026-10-23 test box: CX4 VPI (two functions) in slot 5, the PM1725 in slot 7
    /// on the other virtual switch, a CX4 Lx in slot 6, an M.2 carrier (switch + two NVMe) in slot 4, two SATA
    /// HDDs, a SATA SSD and a USB SSD with no rotation page.
    fn fixture() -> (PathBuf, Paths) {
        let root = crate::os::tests::temp();
        let p = Paths {
            sys: root.join("sys"),
            run: root.join("run"),
        };
        let devs = p.sys("devices/pci0000:00");
        let mk = |path: &str, vendor: &str, device: &str, class: &str| {
            let d = devs.join(path);
            put(d.join("vendor"), vendor);
            put(d.join("device"), device);
            put(d.join("class"), class);
            let name = d.file_name().unwrap().to_owned();
            fs::create_dir_all(p.sys("bus/pci/devices")).unwrap();
            symlink(&d, p.sys("bus/pci/devices").join(name)).unwrap();
            d
        };
        mk("0000:0f:00.0", "0x8086", "0x2030", "0x060400");
        mk("0000:0f:00.0/0000:10:00.0", "0x10b5", "0x8796", "0x060400");
        mk(
            "0000:0f:00.0/0000:10:00.0/0000:11:10.0",
            "0x10b5",
            "0x8796",
            "0x060400",
        );
        mk(
            "0000:0f:00.0/0000:10:00.0/0000:11:10.0/0000:12:00.0",
            "0x15b3",
            "0x1013",
            "0x020700",
        );
        mk(
            "0000:0f:00.0/0000:10:00.0/0000:11:10.0/0000:12:00.1",
            "0x15b3",
            "0x1013",
            "0x020700",
        );
        mk(
            "0000:0f:00.0/0000:10:00.0/0000:11:0c.0",
            "0x10b5",
            "0x8796",
            "0x060400",
        );
        mk(
            "0000:0f:00.0/0000:10:00.0/0000:11:0c.0/0000:69:00.0",
            "0x15b3",
            "0x1015",
            "0x020000",
        );
        let nv = mk(
            "0000:0f:00.0/0000:10:00.0/0000:11:08.0/0000:20:00.0",
            "0x144d",
            "0xa821",
            "0x010802",
        );
        put(nv.join("nvme/nvme1/model"), "PM1725");
        put(nv.join("nvme/nvme1/hwmon4/name"), "nvme");
        put(nv.join("nvme/nvme1/hwmon4/temp1_input"), "30850");
        // slot 4: an M.2 carrier, its switch and two drives behind it
        mk(
            "0000:0f:00.0/0000:10:00.0/0000:11:14.0",
            "0x10b5",
            "0x8796",
            "0x060400",
        );
        let sw = "0000:0f:00.0/0000:10:00.0/0000:11:14.0/0000:30:00.0";
        mk(sw, "0x10b5", "0x8747", "0x060400");
        mk(
            &format!("{sw}/0000:31:08.0"),
            "0x10b5",
            "0x8747",
            "0x060400",
        );
        mk(
            &format!("{sw}/0000:31:08.0/0000:32:00.0"),
            "0x144d",
            "0xa808",
            "0x010802",
        );
        mk(
            &format!("{sw}/0000:31:10.0"),
            "0x10b5",
            "0x8747",
            "0x060400",
        );
        mk(
            &format!("{sw}/0000:31:10.0/0000:33:00.0"),
            "0x144d",
            "0xa808",
            "0x010802",
        );
        // slot 1: an MPX GPU die (never a peripheral) and the Apple flash on the PCH
        mk("0000:06:00.0", "0x8086", "0x2030", "0x060400");
        mk("0000:06:00.0/0000:0b:00.0", "0x1002", "0x66a3", "0x038000");
        let apple = mk("0000:01:00.0", "0x106b", "0x2005", "0x010802");
        put(apple.join("nvme/nvme0/hwmon2/name"), "nvme");
        put(apple.join("nvme/nvme0/hwmon2/temp1_input"), "23000");
        for (slot, addr) in [
            ("1", "0000:06:00"),
            ("4", "0000:11:14"),
            ("5", "0000:11:10"),
            ("6", "0000:11:0c"),
            ("7", "0000:11:08"),
        ] {
            put(p.sys(&format!("bus/pci/slots/{slot}/address")), addr);
        }
        // disks: SATA on the PCH (VPD B1 rotation rate 7200 / 1), USB with no B1 page
        let ahci = devs.join("0000:00:17.0");
        let usb = devs.join("0000:00:14.0/usb2/2-6/2-6:1.0/host0/target0:0:0/0:0:0:0");
        let disks = [
            (
                "sdb",
                ahci.join("ata3/host3/target3:0:0/3:0:0:0"),
                Some(7200u16),
                "1",
            ),
            (
                "sdc",
                ahci.join("ata4/host4/target4:0:0/4:0:0:0"),
                Some(7200),
                "1",
            ),
            (
                "sdd",
                ahci.join("ata5/host5/target5:0:0/5:0:0:0"),
                Some(1),
                "0",
            ),
            ("sda", usb, None, "1"),
        ];
        for (name, dev, rate, rot) in disks {
            put(dev.join("model"), "MODEL");
            if let Some(r) = rate {
                let mut b = vec![0, 0xb1, 0, 0x3c, 0, 0, 0, 2];
                b[4..6].copy_from_slice(&r.to_be_bytes());
                fs::write(dev.join("vpd_pgb1"), b).unwrap();
            }
            put(dev.join("block").join(name).join("dev"), "8:0");
            let b = p.sys(&format!("block/{name}"));
            put(b.join("queue/rotational"), rot);
            symlink(&dev, b.join("device")).unwrap();
            if name == "sdb" || name == "sda" {
                put(dev.join("hwmon/hwmon14/name"), "drivetemp");
                put(dev.join("hwmon/hwmon14/temp1_input"), "35000");
            }
        }
        for (h, target) in [
            ("hwmon2", apple.join("nvme/nvme0/hwmon2")),
            ("hwmon4", nv.join("nvme/nvme1/hwmon4")),
            (
                "hwmon14",
                ahci.join("ata3/host3/target3:0:0/3:0:0:0/hwmon/hwmon14"),
            ),
            ("hwmon15", usb_hwmon(&devs)),
        ] {
            fs::create_dir_all(p.sys("class/hwmon")).unwrap();
            symlink(&target, p.sys("class/hwmon").join(h)).unwrap();
        }
        for h in [
            apple.join("nvme/nvme0/hwmon2"),
            nv.join("nvme/nvme1/hwmon4"),
        ] {
            symlink(h.parent().unwrap(), h.join("device")).unwrap();
        }
        for d in [ahci.join("ata3/host3/target3:0:0/3:0:0:0"), usb_dev(&devs)] {
            symlink(&d, d.join("hwmon/hwmon14/device")).unwrap();
        }
        (root, p)
    }
    fn usb_dev(devs: &Path) -> PathBuf {
        devs.join("0000:00:14.0/usb2/2-6/2-6:1.0/host0/target0:0:0/0:0:0:0")
    }
    fn usb_hwmon(devs: &Path) -> PathBuf {
        usb_dev(devs).join("hwmon/hwmon14")
    }

    #[test]
    fn inventory_classes_slots_and_allowances() {
        let (root, p) = fixture();
        let mut c = Config::default();
        c.slot_zone.insert("4".into(), 3);
        let d = devices(&p, &c);
        let by: BTreeMap<_, _> = d.iter().map(|d| (d.key.as_str(), d)).collect();
        assert_eq!(
            by["0000:12:00"].class, "mellanox",
            "one charge for both ports"
        );
        assert_eq!(by["0000:12:00"].slot.as_deref(), Some("5"));
        assert_eq!(by["0000:69:00"].class, "mellanox");
        assert_eq!(by["0000:20:00"].class, "nvme");
        assert_eq!(by["0000:20:00"].label, "144d:a821 PM1725");
        assert_eq!(
            by["0000:30:00"].class, "pcie-switch",
            "the carrier's switch, once"
        );
        assert_eq!(by["0000:32:00"].class, "nvme-switched");
        assert_eq!(by["0000:33:00"].class, "nvme-switched");
        assert_eq!(by["sdb"].class, "disk");
        assert_eq!(by["sdd"].class, "ssd", "rotation rate 1 wins over nothing");
        assert_eq!(
            by["sda"].class, "usb-disk",
            "USB rotational=1 is not evidence"
        );
        assert_eq!(by["sda"].zone, None);
        assert_eq!(by["sdb"].zone, Some(c.disk_zone));
        assert!(
            !by.contains_key("0000:0b:00"),
            "GPU dies are not peripherals"
        );
        assert!(
            !by.contains_key("0000:01:00"),
            "the Apple flash is in the base"
        );
        assert!(
            !by.contains_key("0000:11:10"),
            "a slot port itself is not a card"
        );
        assert!(
            !by.contains_key("0000:31:08"),
            "switch downstream ports are not charged"
        );
        assert_eq!(d.len(), 10);
        let total: f64 = d.iter().map(|d| d.watts).sum();
        let w = |k: &str| c.device_w[k];
        assert_eq!(
            total,
            2.0 * w("mellanox")
                + w("nvme")
                + w("pcie-switch")
                + 2.0 * w("nvme-switched")
                + 2.0 * w("disk")
                + w("ssd")
                + w("usb-disk")
        );
        // a PCI-id allowance outranks the class
        c.device_pci_w.insert("15b3:1015".into(), 11.0);
        assert_eq!(
            devices(&p, &c)
                .iter()
                .find(|d| d.key == "0000:69:00")
                .unwrap()
                .watts,
            11.0
        );
        // slots outside SLOT_ZONE are not peripheral slots
        c.slot_zone.remove("4");
        assert_eq!(devices(&p, &c).len(), 7);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sensors_zone_by_slot_and_disk_bay_and_skip_usb() {
        let (root, p) = fixture();
        let mut c = Config::default();
        c.slot_zone.insert("7".into(), 2);
        c.disk_zone = 4;
        let s = sensors(&p, &c);
        let by: BTreeMap<_, _> = s.iter().map(|s| (s.key.as_str(), s)).collect();
        assert_eq!(by.len(), 3, "{:?}", by.keys().collect::<Vec<_>>());
        assert_eq!(
            by["nvme"].spec.fan, c.nvme.fan,
            "the Apple flash keeps NVME"
        );
        assert_eq!(by["nvme:nvme1"].spec.fan, 2, "slot 7's zone");
        assert_eq!(by["nvme:nvme1"].device.as_deref(), Some("0000:20:00"));
        assert_eq!(by["drivetemp:3:0:0:0"].spec.fan, 4, "the disk bay's zone");
        assert_eq!(by["drivetemp:3:0:0:0"].device.as_deref(), Some("sdb"));
        assert_eq!(
            by["drivetemp:3:0:0:0"].spec.emergency,
            c.periph["drivetemp"].2 - 5.0,
            "no usable crit: the configured one"
        );
        assert!(
            hwmon_input(&by["drivetemp:3:0:0:0"].dev, "drivetemp").is_some(),
            "hwmon/hwmonN layout"
        );
        assert!(
            hwmon_input(&by["nvme:nvme1"].dev, "nvme").is_some(),
            "hwmonN child layout"
        );
        // the CX4s have no sensor: their zone keeps the UNMONITORED floor, the monitored ones do not
        c.unmonitored.insert("mellanox".into(), (900.0, Some(4)));
        c.unmonitored.insert("nvme".into(), (1300.0, None));
        let d = devices(&p, &c);
        let mon: Vec<String> = s.iter().filter_map(|s| s.device.clone()).collect();
        let f = unmonitored_floors(&c, &d, &mon);
        let z = c.slot_zone["5"] - 1;
        assert_eq!(f.rpm[z], 900.0);
        assert_eq!(f.rpm[1], 0.0, "the PM1725 in slot 7 (zone 2) is monitored");
        assert_eq!(f.of(z, &[600.0, 1150.0, 800.0, 500.0]), 900.0);
        assert_eq!(
            f.of(z, &[600.0, 1150.0, 800.0, 1400.0]),
            1400.0,
            "fan 3 follows fan 4"
        );
        assert_eq!(f.follow_lists()[&(z + 1).to_string()], vec![4]);
        // TEMP_COMMAND: one command sensor per Mellanox card without hwmon, in its slot's zone
        let prog = root.join("mget_temp");
        put(&prog, "#!/bin/sh\necho 43\n");
        c.temp_command.insert(
            "mellanox".into(),
            vec![
                prog.to_string_lossy().into_owned(),
                "-d".into(),
                "{pci}".into(),
            ],
        );
        let cs = command_sensors(&p, &c, &d, &s);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].key, "mellanox:0000:12:00");
        assert_eq!(cs[0].spec.fan, c.slot_zone["5"]);
        assert_eq!(cs[0].command.as_ref().unwrap()[2], "0000:12:00.0");
        c.temp_command
            .insert("mellanox".into(), vec!["/nonexistent/mget_temp".into()]);
        assert!(
            command_sensors(&p, &c, &d, &s).is_empty(),
            "a missing program adds nothing"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
