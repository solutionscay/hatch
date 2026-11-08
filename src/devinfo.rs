//! Device info: identity, battery, storage. Read-only lockdown GetValue calls.
//! Also holds the sidebar card and its refresh loop.

use crate::device;
use crate::{Hatch, dim, faint, ink, line, mono, rt, wash};
use anyhow::Result;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::*;
use idevice::lockdown::LockdownClient;
use plist::{Dictionary, Value};
use serde::Serialize;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct DeviceInfo {
    pub udid: String,
    pub name: String,
    pub model: String,
    pub model_number: String,
    pub ios: String,
    pub build: String,
    pub serial: String,
    /// Percent, 0-100. None if the phone did not report it.
    pub battery_level: Option<u64>,
    pub charging: Option<bool>,
    pub external_power: Option<bool>,
    pub fully_charged: Option<bool>,
    /// Bytes. Total is the disk size. Free is what the user can still use.
    pub storage_total: Option<u64>,
    pub storage_data_total: Option<u64>,
    pub storage_free: Option<u64>,
}

async fn domain(l: &mut LockdownClient, d: &str) -> Dictionary {
    l.get_value(None, Some(d)).await.ok().and_then(|v| v.into_dictionary()).unwrap_or_default()
}

fn s(d: &Dictionary, k: &str) -> String {
    d.get(k).and_then(Value::as_string).unwrap_or_default().to_string()
}

fn n(d: &Dictionary, k: &str) -> Option<u64> {
    d.get(k).and_then(|v| v.as_unsigned_integer().or_else(|| v.as_signed_integer().map(|i| i.max(0) as u64)))
}

fn b(d: &Dictionary, k: &str) -> Option<bool> {
    d.get(k).and_then(Value::as_boolean)
}

/// Read identity, battery and storage. Only GetValue requests: nothing changes on the phone.
pub async fn device_info(udid: &str) -> Result<DeviceInfo> {
    let p = device::provider_for(udid).await?;
    let mut l = device::session(&p).await?;
    let id = l.get_value(None, None).await?.into_dictionary().unwrap_or_default();
    let bat = domain(&mut l, "com.apple.mobile.battery").await;
    let disk = domain(&mut l, "com.apple.disk_usage").await;
    let udid_v = s(&id, "UniqueDeviceID");
    Ok(DeviceInfo {
        udid: if udid_v.is_empty() { udid.to_string() } else { udid_v },
        name: s(&id, "DeviceName"),
        model: s(&id, "ProductType"),
        model_number: format!("{}{}", s(&id, "ModelNumber"), s(&id, "RegionInfo")),
        ios: s(&id, "ProductVersion"),
        build: s(&id, "BuildVersion"),
        serial: s(&id, "SerialNumber"),
        battery_level: n(&bat, "BatteryCurrentCapacity"),
        charging: b(&bat, "BatteryIsCharging"),
        external_power: b(&bat, "ExternalConnected"),
        fully_charged: b(&bat, "FullyCharged"),
        storage_total: n(&disk, "TotalDiskCapacity"),
        storage_data_total: n(&disk, "TotalDataCapacity"),
        storage_free: n(&disk, "AmountDataAvailable").or(n(&disk, "TotalDataAvailable")),
    })
}

/// Decimal units, as iOS Settings shows storage.
fn gb(n: u64) -> String {
    let g = n as f64 / 1e9;
    if g >= 100.0 { format!("{g:.0} GB") } else { format!("{g:.1} GB") }
}

fn bar(frac: f32, color: Rgba) -> Div {
    let frac = frac.clamp(0.0, 1.0);
    div().h(px(4.)).w_full().bg(wash()).child(div().h_full().w(relative(frac)).bg(color))
}

fn row(label: &'static str, value: String) -> Div {
    h_flex()
        .h(px(18.))
        .gap_2()
        .child(div().w(px(56.)).flex_none().text_color(faint()).child(label))
        .child(div().flex_1().min_w_0().truncate().text_color(dim()).child(value))
}

const REFRESH: Duration = Duration::from_secs(30);

impl Hatch {
    /// Keep `self.info` fresh for the selected, trusted phone. Fetch at once when the
    /// selection changes, then every 30 s for battery and storage.
    pub(crate) fn start_info_poll(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut last: Option<(String, Instant)> = None;
            loop {
                let Ok(want) = this.update(cx, |this, _| {
                    this.current().filter(|d| d.paired).map(|d| d.udid.clone())
                }) else {
                    break;
                };
                let due = match (&want, &last) {
                    (None, _) => false,
                    (Some(u), Some((lu, at))) => u != lu || at.elapsed() >= REFRESH,
                    (Some(_), None) => true,
                };
                if want.is_none() {
                    last = None;
                    let alive = this
                        .update(cx, |this, cx| {
                            if this.info.take().is_some() {
                                cx.notify();
                            }
                        })
                        .is_ok();
                    if !alive {
                        break;
                    }
                }
                if let (true, Some(u)) = (due, want) {
                    last = Some((u.clone(), Instant::now()));
                    let r = rt().spawn({
                        let u = u.clone();
                        async move { device_info(&u).await }
                    })
                    .await;
                    let alive = this
                        .update(cx, |this, cx| {
                            // Keep the last good reading on a failed refresh of the same phone.
                            match r {
                                Ok(Ok(i)) => {
                                    if this.info.as_ref() != Some(&i) {
                                        this.info = Some(i);
                                        cx.notify();
                                    }
                                }
                                _ if this.info.as_ref().is_some_and(|i| i.udid != u) => {
                                    this.info = None;
                                    cx.notify();
                                }
                                _ => {}
                            }
                        })
                        .is_ok();
                    if !alive {
                        break;
                    }
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
        .detach();
    }

    /// Small battery + storage card for the sidebar. None until the first reading arrives.
    pub(crate) fn info_card(&self) -> Option<Div> {
        let i = self.info.as_ref()?;
        if self.udid.as_deref() != Some(i.udid.as_str()) {
            return None;
        }
        let mut card = v_flex().px_4().py_3().gap_2().text_size(px(11.)).border_b_1().border_color(line());

        if let Some(level) = i.battery_level {
            let state = if i.fully_charged == Some(true) && i.external_power == Some(true) {
                "Charged"
            } else if i.charging == Some(true) {
                "Charging"
            } else if i.external_power == Some(true) {
                "Plugged in"
            } else {
                "On battery"
            };
            let color = if level <= 20 && i.charging != Some(true) { crate::cor() } else { crate::acc() };
            card = card.child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(div().text_color(faint()).child("Battery"))
                            .child(div().font_family(mono()).text_color(ink()).child(format!("{level}% · {state}"))),
                    )
                    .child(bar(level as f32 / 100.0, color)),
            );
        }

        let total = i.storage_data_total.or(i.storage_total);
        if let (Some(total), Some(free)) = (total, i.storage_free) {
            let used = total.saturating_sub(free);
            let frac = if total > 0 { used as f32 / total as f32 } else { 0.0 };
            let color = if frac >= 0.9 { crate::amb() } else { dim() };
            card = card.child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(div().text_color(faint()).child("Storage"))
                            .child(div().font_family(mono()).text_color(ink()).child(format!("{} free", gb(free)))),
                    )
                    .child(bar(frac, color))
                    .child(div().text_color(faint()).child(format!("{} used of {}", gb(used), gb(total)))),
            );
        }

        if !i.serial.is_empty() {
            card = card.child(row("Serial", i.serial.clone()));
        }
        if !i.build.is_empty() {
            card = card.child(row("Build", i.build.clone()));
        }
        Some(card)
    }
}
