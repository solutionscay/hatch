//! Photos library search. Copies PhotoData/Photos.sqlite (with -wal and -shm) from the
//! media partition to a private temp folder, queries the copy read-only, then deletes it.
//! The phone's database is never opened in place and never written.

use crate::device::{self, Root};
use anyhow::{Context, Result, anyhow, bail};
use chrono::{Local, NaiveDate, NaiveTime, TimeZone};
use idevice::{IdeviceError, afc::errors::AfcError};
use rusqlite::{Connection, OpenFlags, params_from_iter, types::Value as Sql};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const DB: &str = "/PhotoData/Photos.sqlite";
/// Core Data stores dates as seconds since 2001-01-01 UTC.
const COREDATA_EPOCH: f64 = 978_307_200.0;
const DEFAULT_LIMIT: u64 = 200;
const MAX_LIMIT: u64 = 2000;

/// MCP tool definitions for this module.
pub fn tools() -> Vec<Value> {
    let dev = json!({"type":"string","description":"Device UDID. Defaults to the first connected device."});
    vec![
        json!({"name":"search_photos",
            "description":"Search the iPhone Photos library by date range, album, favorite, and location. All filters are optional and combine with AND. Returns newest first. Each result has a path in root 'media' (under /DCIM) that read_file and download_file accept. Hidden and recently deleted items are left out, and so are items stored outside /DCIM.",
            "inputSchema":{"type":"object","properties":{
                "udid":dev,
                "from":{"type":"string","description":"Start date, inclusive. YYYY-MM-DD (local time) or RFC 3339."},
                "to":{"type":"string","description":"End date, inclusive. YYYY-MM-DD (local time, whole day) or RFC 3339."},
                "album":{"type":"string","description":"Album title (case-insensitive exact match). Use list_photo_albums for titles."},
                "favorite":{"type":"boolean","description":"true: favorites only. false: non-favorites only."},
                "near":{"type":"object","description":"Only items taken within radius_km of this point.",
                    "properties":{"lat":{"type":"number"},"lon":{"type":"number"},"radius_km":{"type":"number","description":"Default 1"}},
                    "required":["lat","lon"]},
                "has_location":{"type":"boolean","description":"true: only items with GPS data. false: only items without."},
                "media_type":{"type":"string","enum":["photo","video"]},
                "limit":{"type":"integer","description":"Default 200, max 2000"}
            }}}),
        json!({"name":"list_photo_albums",
            "description":"List the albums the user made in the iPhone Photos library, with item counts.",
            "inputSchema":{"type":"object","properties":{"udid":dev}}}),
    ]
}

pub fn handles(name: &str) -> bool {
    matches!(name, "search_photos" | "list_photo_albums")
}

pub async fn call(udid: &str, name: &str, args: &Value) -> Result<Value> {
    let q = match name {
        "search_photos" => Some(Query::parse(args)?),
        _ => None,
    };
    let lib = Snapshot::take(udid).await?;
    let path = lib.db.clone();
    tokio::task::spawn_blocking(move || {
        let _keep = lib; // deleted when this closure ends
        let conn = open(&path)?;
        match &q {
            Some(q) => search(&conn, q),
            None => albums(&conn),
        }
    })
    .await?
}

/// A private copy of the Photos database. Removed on drop.
struct Snapshot {
    dir: PathBuf,
    db: PathBuf,
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Snapshot {
    async fn take(udid: &str) -> Result<Snapshot> {
        let dir = std::env::temp_dir().join(format!("hatch-photos-{}", uuid::Uuid::new_v4().simple()));
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(&dir).context("could not make a temp folder for the Photos database")?;
        let snap = Snapshot { db: dir.join("Photos.sqlite"), dir };
        device::copy_to_local(udid, &Root::Media, DB, &snap.db)
            .await
            .map_err(|e| db_error(e, "Photos.sqlite"))?;
        for ext in ["-wal", "-shm"] {
            let local = snap.dir.join(format!("Photos.sqlite{ext}"));
            match device::copy_to_local(udid, &Root::Media, &format!("{DB}{ext}"), &local).await {
                Ok(_) => {}
                Err(e) if afc_code(&e) == Some(AfcErrorKind::NotFound) => {
                    let _ = std::fs::remove_file(&local);
                }
                Err(e) => return Err(db_error(e, &format!("Photos.sqlite{ext}"))),
            }
        }
        Ok(snap)
    }
}

#[derive(PartialEq)]
enum AfcErrorKind {
    NotFound,
    Denied,
}

fn afc_code(e: &anyhow::Error) -> Option<AfcErrorKind> {
    e.chain().find_map(|c| match c.downcast_ref::<IdeviceError>() {
        Some(IdeviceError::Afc(AfcError::ObjectNotFound)) => Some(AfcErrorKind::NotFound),
        Some(IdeviceError::Afc(AfcError::PermDenied)) => Some(AfcErrorKind::Denied),
        _ => None,
    })
}

fn db_error(e: anyhow::Error, file: &str) -> anyhow::Error {
    match afc_code(&e) {
        Some(AfcErrorKind::NotFound) => anyhow!(
            "The Photos library database is missing ({DB}). \
             The iPhone may have no photos yet, or iOS does not expose the library over USB."
        ),
        Some(AfcErrorKind::Denied) => anyhow!(
            "iOS denied access to {file}. The Photos database is locked.\n\
             Unlock the iPhone with the passcode. Then try again."
        ),
        // Keep the original error so device::explain can report locked / unplugged phones.
        None => e.context(format!("could not copy {file} from the iPhone")),
    }
}

fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(sql_error)?;
    // Fail fast on a bad copy instead of on the first query.
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))
        .map_err(sql_error)?;
    for (t, c) in [("ZASSET", "ZDATECREATED"), ("ZASSET", "ZDIRECTORY"), ("ZASSET", "ZFILENAME")] {
        if !has_column(&conn, t, c)? {
            bail!("This iOS version uses a Photos database layout Hatch does not know ({t}.{c} is missing).");
        }
    }
    Ok(conn)
}

fn sql_error(e: rusqlite::Error) -> anyhow::Error {
    use rusqlite::ErrorCode::*;
    match e.sqlite_error_code() {
        Some(DatabaseBusy | DatabaseLocked) => {
            anyhow!("The Photos database is locked. The Photos app may be writing to it. Try again in a few seconds.")
        }
        Some(NotADatabase | DatabaseCorrupt) => anyhow!(
            "The copy of the Photos database is not readable. The Photos app may have been writing to it during the copy. \
             Try again in a few seconds. If it fails again, unlock the iPhone and try again."
        ),
        _ => anyhow!("Photos database error: {e}"),
    }
}

fn has_column(conn: &Connection, table: &str, col: &str) -> Result<bool> {
    Ok(columns(conn, table)?.iter().any(|c| c == col))
}

fn columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut st = conn.prepare(&format!("PRAGMA table_info(\"{table}\")")).map_err(sql_error)?;
    let cols = st
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    Ok(cols)
}

/// The album/asset join table. Its name has a Core Data entity number that changes
/// between iOS versions (iOS 26: Z_33ASSETS with columns Z_33ALBUMS and Z_3ASSETS).
fn album_join(conn: &Connection) -> Result<(String, String, String)> {
    let mut st = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name GLOB 'Z_[0-9]*ASSETS'")
        .map_err(sql_error)?;
    let names: Vec<String> = st
        .query_map([], |r| r.get(0))
        .map_err(sql_error)?
        .collect::<Result<_, _>>()
        .map_err(sql_error)?;
    for t in names {
        // Only Z_<n>ASSETS. Skips tables such as Z_3MEMORIESBEINGCURATEDASSETS.
        let n = &t[2..t.len() - 6];
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let cols = columns(conn, &t)?;
        let albums = format!("Z_{n}ALBUMS");
        let assets = cols.iter().find(|c| c.starts_with("Z_") && !c.starts_with("Z_FOK") && c.ends_with("ASSETS"));
        if let (true, Some(b)) = (cols.contains(&albums), assets) {
            return Ok((t.clone(), albums, b.clone()));
        }
    }
    bail!("This iOS version uses a Photos database layout Hatch does not know (no album-to-asset table).")
}

struct Query {
    from: Option<f64>,
    to: Option<f64>,
    album: Option<String>,
    favorite: Option<bool>,
    near: Option<(f64, f64, f64)>,
    has_location: Option<bool>,
    kind: Option<i64>,
    limit: u64,
}

/// Parse a date to a Core Data timestamp. Date-only input uses local time; `end` picks
/// the end of that day so the range is inclusive.
fn parse_date(s: &str, end: bool) -> Result<f64> {
    let unix = if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        d.timestamp() as f64
    } else if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let t = if end { NaiveTime::from_hms_opt(23, 59, 59).unwrap() } else { NaiveTime::MIN };
        let dt = Local
            .from_local_datetime(&d.and_time(t))
            .earliest()
            .ok_or_else(|| anyhow!("{s} is not a valid local time"))?;
        dt.timestamp() as f64 + if end { 0.999 } else { 0.0 }
    } else {
        bail!("Bad date {s:?}. Use YYYY-MM-DD or RFC 3339 (2025-06-01T09:00:00Z).");
    };
    Ok(unix - COREDATA_EPOCH)
}

impl Query {
    fn parse(a: &Value) -> Result<Query> {
        let date = |k: &str, end| a[k].as_str().map(|s| parse_date(s, end)).transpose();
        let near = match &a["near"] {
            Value::Null => None,
            n => {
                let lat = n["lat"].as_f64().ok_or_else(|| anyhow!("near.lat is required"))?;
                let lon = n["lon"].as_f64().ok_or_else(|| anyhow!("near.lon is required"))?;
                let r = n["radius_km"].as_f64().unwrap_or(1.0);
                if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) || r <= 0.0 {
                    bail!("near needs lat in -90..90, lon in -180..180, and radius_km > 0");
                }
                Some((lat, lon, r))
            }
        };
        let kind = match a["media_type"].as_str() {
            None => None,
            Some("photo") => Some(0),
            Some("video") => Some(1),
            Some(o) => bail!("media_type must be 'photo' or 'video', not {o:?}"),
        };
        let q = Query {
            from: date("from", false)?,
            to: date("to", true)?,
            album: a["album"].as_str().map(str::to_string),
            favorite: a["favorite"].as_bool(),
            near,
            has_location: a["has_location"].as_bool(),
            kind,
            limit: a["limit"].as_u64().unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        };
        if let (Some(f), Some(t)) = (q.from, q.to)
            && f > t
        {
            bail!("from is after to");
        }
        Ok(q)
    }
}

/// ZGENERICALBUM.ZKIND 2 is an album the user made. Other kinds are smart albums,
/// folders, and internal iOS albums.
const USER_ALBUM: &str = "ZKIND = 2";

/// Assets without GPS have lat/lon -180 (or NULL).
const HAS_GPS: &str = "(a.ZLATITUDE IS NOT NULL AND a.ZLATITUDE BETWEEN -90 AND 90 AND a.ZLONGITUDE BETWEEN -180 AND 180 AND NOT (a.ZLATITUDE = -180 OR a.ZLONGITUDE = -180))";

fn search(conn: &Connection, q: &Query) -> Result<Value> {
    let acols = columns(conn, "ZASSET")?;
    let has = |c: &str| acols.iter().any(|x| x == c);
    let opt = |c: &str| if has(c) { format!("a.{c}") } else { "NULL".into() };
    let aa = has_column(conn, "ZADDITIONALASSETATTRIBUTES", "ZORIGINALFILENAME")?;

    let mut sql = format!(
        "SELECT a.ZDIRECTORY, a.ZFILENAME, a.ZDATECREATED, {}, {}, {}, {}, {}, {}, {} FROM ZASSET a {} WHERE a.ZDIRECTORY LIKE 'DCIM/%' AND a.ZFILENAME IS NOT NULL",
        opt("ZFAVORITE"),
        opt("ZLATITUDE"),
        opt("ZLONGITUDE"),
        opt("ZKIND"),
        opt("ZWIDTH"),
        opt("ZHEIGHT"),
        if aa { "aa.ZORIGINALFILENAME" } else { "NULL" },
        if aa { "LEFT JOIN ZADDITIONALASSETATTRIBUTES aa ON aa.ZASSET = a.Z_PK" } else { "" },
    );
    let mut p: Vec<Sql> = Vec::new();
    if has("ZTRASHEDSTATE") {
        sql += " AND a.ZTRASHEDSTATE = 0";
    }
    if has("ZHIDDEN") {
        sql += " AND a.ZHIDDEN = 0";
    }
    if let Some(f) = q.from {
        sql += " AND a.ZDATECREATED >= ?";
        p.push(Sql::Real(f));
    }
    if let Some(t) = q.to {
        sql += " AND a.ZDATECREATED <= ?";
        p.push(Sql::Real(t));
    }
    if let Some(fav) = q.favorite {
        if !has("ZFAVORITE") {
            bail!("This Photos database has no favorite flag (ZASSET.ZFAVORITE).");
        }
        sql += if fav { " AND a.ZFAVORITE = 1" } else { " AND COALESCE(a.ZFAVORITE, 0) = 0" };
    }
    if let Some(k) = q.kind {
        sql += " AND a.ZKIND = ?";
        p.push(Sql::Integer(k));
    }
    let wants_gps = q.near.is_some() || q.has_location.is_some();
    if wants_gps && !(has("ZLATITUDE") && has("ZLONGITUDE")) {
        bail!("This Photos database has no location columns (ZASSET.ZLATITUDE/ZLONGITUDE).");
    }
    match q.has_location {
        Some(true) => sql += &format!(" AND {HAS_GPS}"),
        Some(false) => sql += &format!(" AND NOT {HAS_GPS}"),
        None => {}
    }
    if let Some((lat, lon, r)) = q.near {
        // Bounding box in SQL, exact distance below.
        let dlat = r / 111.0;
        let dlon = r / (111.0 * lat.to_radians().cos().abs().max(0.01));
        sql += &format!(" AND {HAS_GPS} AND a.ZLATITUDE BETWEEN ? AND ?");
        p.extend([Sql::Real(lat - dlat), Sql::Real(lat + dlat)]);
        if dlon < 180.0 {
            let (lo, hi) = (lon - dlon, lon + dlon);
            if lo < -180.0 || hi > 180.0 {
                // Box crosses the antimeridian.
                sql += " AND (a.ZLONGITUDE >= ? OR a.ZLONGITUDE <= ?)";
                p.extend([Sql::Real(wrap(lo)), Sql::Real(wrap(hi))]);
            } else {
                sql += " AND a.ZLONGITUDE BETWEEN ? AND ?";
                p.extend([Sql::Real(lo), Sql::Real(hi)]);
            }
        }
    }
    if let Some(title) = &q.album {
        let (t, acol, scol) = album_join(conn)?;
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM ZGENERICALBUM WHERE ZTITLE = ? COLLATE NOCASE AND {USER_ALBUM}"), [title], |r| {
                r.get(0)
            })
            .map_err(sql_error)?;
        if n == 0 {
            bail!("No album named {title:?}. Use list_photo_albums to see album titles.");
        }
        let trashed = if has_column(conn, "ZGENERICALBUM", "ZTRASHEDSTATE")? { " AND g.ZTRASHEDSTATE = 0" } else { "" };
        sql += &format!(
            " AND a.Z_PK IN (SELECT j.\"{scol}\" FROM \"{t}\" j JOIN ZGENERICALBUM g ON g.Z_PK = j.\"{acol}\" WHERE g.ZTITLE = ? COLLATE NOCASE AND g.{USER_ALBUM}{trashed})"
        );
        p.push(Sql::Text(title.clone()));
    }
    sql += " ORDER BY a.ZDATECREATED DESC";
    // With a radius filter the exact check drops rows after SQL, so do not LIMIT in SQL.
    if q.near.is_none() {
        sql += &format!(" LIMIT {}", q.limit + 1);
    }

    let mut st = conn.prepare(&sql).map_err(sql_error)?;
    let mut rows = st.query(params_from_iter(p)).map_err(sql_error)?;
    let mut out = Vec::new();
    let mut more = false;
    while let Some(r) = rows.next().map_err(sql_error)? {
        // Columns: 0 dir, 1 file, 2 date, 3 favorite, 4 lat, 5 lon, 6 kind, 7 width, 8 height, 9 original name.
        let lat: Option<f64> = r.get(4).ok().flatten();
        let lon: Option<f64> = r.get(5).ok().flatten();
        let gps = match (lat, lon) {
            (Some(la), Some(lo)) if (-90.0..=90.0).contains(&la) && (-180.0..=180.0).contains(&lo) && la != -180.0 && lo != -180.0 => Some((la, lo)),
            _ => None,
        };
        let mut dist = None;
        if let Some((nlat, nlon, rad)) = q.near {
            let Some((la, lo)) = gps else { continue };
            let d = haversine_km(nlat, nlon, la, lo);
            if d > rad {
                continue;
            }
            dist = Some((d * 1000.0).round() / 1000.0);
        }
        if out.len() as u64 >= q.limit {
            more = true;
            break;
        }
        let dir: String = r.get(0).map_err(sql_error)?;
        let file: String = r.get(1).map_err(sql_error)?;
        let created: Option<f64> = r.get(2).ok().flatten();
        let kind: Option<i64> = r.get(6).ok().flatten();
        let mut item = json!({
            "path": media_path(&dir, &file),
            "root": "media",
            "date": created.and_then(fmt_date),
            "favorite": r.get::<_, Option<i64>>(3).ok().flatten().unwrap_or(0) == 1,
            "type": match kind { Some(0) => "photo", Some(1) => "video", _ => "other" },
            "width": r.get::<_, Option<i64>>(7).ok().flatten(),
            "height": r.get::<_, Option<i64>>(8).ok().flatten(),
            "original_name": r.get::<_, Option<String>>(9).ok().flatten(),
        });
        if let Some((la, lo)) = gps {
            item["lat"] = json!(la);
            item["lon"] = json!(lo);
        }
        if let Some(d) = dist {
            item["distance_km"] = json!(d);
        }
        out.push(item);
    }
    Ok(json!({ "count": out.len(), "more": more, "photos": out }))
}

fn wrap(lon: f64) -> f64 {
    if lon < -180.0 { lon + 360.0 } else if lon > 180.0 { lon - 360.0 } else { lon }
}

fn haversine_km(a_lat: f64, a_lon: f64, b_lat: f64, b_lon: f64) -> f64 {
    let (p1, p2) = (a_lat.to_radians(), b_lat.to_radians());
    let dp = p2 - p1;
    let dl = (b_lon - a_lon).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6371.0088 * h.sqrt().asin()
}

/// ZDIRECTORY is relative to the media root for camera roll items ("DCIM/100APPLE").
fn media_path(dir: &str, file: &str) -> String {
    let dir = dir.trim_matches('/');
    if dir.is_empty() { format!("/{file}") } else { format!("/{dir}/{file}") }
}

fn fmt_date(cd: f64) -> Option<String> {
    let unix = cd + COREDATA_EPOCH;
    let secs = unix.floor() as i64;
    Local.timestamp_opt(secs, 0).single().map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
}

fn albums(conn: &Connection) -> Result<Value> {
    let gcols = columns(conn, "ZGENERICALBUM")?;
    if !gcols.iter().any(|c| c == "ZTITLE") {
        bail!("This iOS version uses a Photos database layout Hatch does not know (ZGENERICALBUM.ZTITLE is missing).");
    }
    let has = |c: &str| gcols.iter().any(|x| x == c);
    if !has("ZKIND") {
        bail!("This iOS version uses a Photos database layout Hatch does not know (ZGENERICALBUM.ZKIND is missing).");
    }
    let (t, acol, scol) = album_join(conn)?;
    // One row per title: search_photos matches by title, so same-named albums merge.
    // Count only items search_photos can return.
    let trashed = if has("ZTRASHEDSTATE") { " AND g.ZTRASHEDSTATE = 0" } else { "" };
    let sql = format!(
        "SELECT min(g.ZTITLE), count(DISTINCT a.Z_PK) FROM ZGENERICALBUM g \
         LEFT JOIN \"{t}\" j ON j.\"{acol}\" = g.Z_PK \
         LEFT JOIN ZASSET a ON a.Z_PK = j.\"{scol}\" AND a.ZDIRECTORY LIKE 'DCIM/%' AND a.ZTRASHEDSTATE = 0 AND a.ZHIDDEN = 0 \
         WHERE g.ZTITLE IS NOT NULL AND g.ZTITLE != '' AND g.{USER_ALBUM}{trashed} \
         GROUP BY g.ZTITLE COLLATE NOCASE ORDER BY g.ZTITLE COLLATE NOCASE"
    );
    let mut st = conn.prepare(&sql).map_err(sql_error)?;
    let rows = st
        .query_map([], |r| Ok(json!({ "title": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)? })))
        .map_err(sql_error)?;
    let out = rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)?;
    Ok(json!({ "count": out.len(), "albums": out }))
}
