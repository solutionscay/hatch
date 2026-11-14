//! Device info: identity, battery, storage. Read-only lockdown GetValue calls.
//! Also holds the sidebar widget, the Settings rows, and the refresh loop.

use crate::device;
use crate::{Hatch, dim, faint, ink, line, mono, rt, wash};
use anyhow::Result;
use gpui_kit::component::{Icon, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
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
    if crate::demo::on() {
        return Ok(crate::demo::info());
    }
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

    /// The reading for the selected phone, if it has arrived.
    fn current_info(&self) -> Option<&DeviceInfo> {
        self.info.as_ref().filter(|i| self.udid.as_deref() == Some(i.udid.as_str()))
    }

    /// Two short lines under the device header: battery and free storage.
    /// None until the first reading arrives.
    pub(crate) fn info_widget(&self) -> Option<Div> {
        let i = self.current_info()?;
        let mut w = v_flex().px_4().pb_3().gap_1().text_size(px(11.)).font_family(mono());

        if let Some(level) = i.battery_level {
            let charging = i.charging == Some(true);
            let low = level <= 20 && !charging;
            let glyph = if low {
                "icons/battery-low.svg"
            } else if level <= 60 {
                "icons/battery-medium.svg"
            } else {
                "icons/battery-full.svg"
            };
            let color = if low { crate::cor() } else { dim() };
            w = w.child(
                h_flex()
                    .h(px(18.))
                    .gap_2()
                    .items_center()
                    .child(Icon::empty().path(glyph).xsmall().text_color(color))
                    .child(div().text_color(if low { color } else { ink() }).child(format!("{level}%")))
                    .when(charging || i.external_power == Some(true), |d| {
                        d.child(Icon::empty().path("icons/zap.svg").xsmall().text_color(if charging { crate::acc() } else { faint() }))
                    }),
            );
        }

        let total = i.storage_data_total.or(i.storage_total);
        if let (Some(total), Some(free)) = (total, i.storage_free) {
            let used = total.saturating_sub(free);
            let frac = if total > 0 { used as f32 / total as f32 } else { 0.0 };
            let full = frac >= 0.9;
            w = w.child(
                h_flex()
                    .h(px(18.))
                    .gap_2()
                    .items_center()
                    .child(Icon::empty().path("icons/hard-drive.svg").xsmall().text_color(if full { crate::amb() } else { dim() }))
                    .child(div().flex_none().text_color(if full { crate::amb() } else { ink() }).child(format!("{} free", gb(free))))
                    .child(div().flex_1().min_w(px(24.)).child(bar(frac, if full { crate::amb() } else { faint() }))),
            );
        }
        Some(w)
    }

    /// Identity rows for Settings > Device.
    pub(crate) fn device_section(&self, cx: &mut Context<Self>) -> Div {
        let mut sec = v_flex();
        let Some(i) = self.current_info() else {
            return sec.child(div().px_6().py_3().text_size(px(12.)).text_color(faint()).child("Details appear here once an iPhone is connected and trusted."));
        };
        let total = i.storage_data_total.or(i.storage_total);
        let battery = i.battery_level.map(|l| {
            let state = if i.fully_charged == Some(true) && i.external_power == Some(true) {
                "Charged"
            } else if i.charging == Some(true) {
                "Charging"
            } else if i.external_power == Some(true) {
                "Plugged in"
            } else {
                "On battery"
            };
            format!("{l}% · {state}")
        });
        let storage = match (total, i.storage_free) {
            (Some(t), Some(f)) => Some(format!("{} free · {} used of {}", gb(f), gb(t.saturating_sub(f)), gb(t))),
            _ => None,
        };
        let rows: [(&'static str, &'static str, Option<String>); 9] = [
            ("dev-name", "Name", Some(i.name.clone())),
            ("dev-model", "Model", Some(i.model.clone())),
            ("dev-modelno", "Model number", Some(i.model_number.clone())),
            ("dev-ios", "iOS version", Some(i.ios.clone())),
            ("dev-build", "Build", Some(i.build.clone())),
            ("dev-serial", "Serial", Some(i.serial.clone())),
            ("dev-udid", "UDID", Some(i.udid.clone())),
            ("dev-battery", "Battery", battery),
            ("dev-storage", "Storage", storage),
        ];
        for (id, label, value) in rows {
            let Some(value) = value.filter(|v| !v.is_empty()) else { continue };
            let copy = matches!(id, "dev-serial" | "dev-udid");
            sec = sec.child(
                h_flex()
                    .h(px(40.))
                    .px_6()
                    .gap_3()
                    .items_center()
                    .border_b_1()
                    .border_color(line())
                    .child(div().w(px(112.)).flex_none().text_color(faint()).child(label))
                    .child(div().flex_1().min_w_0().truncate().font_family(mono()).text_size(px(12.)).text_color(ink()).child(value.clone()))
                    .when(copy, |d| d.child(self.copy_btn(id, value, cx))),
            );
        }
        sec
    }
}
