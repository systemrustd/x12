use std::{collections::HashMap, io};

use drm::control::{
    AtomicCommitFlags, Device as ControlDevice, Mode as DrmMode, ModeTypeFlags, PlaneType,
    atomic::AtomicModeReq, connector, crtc, framebuffer, plane, property,
};

use crate::drm::Device;

#[derive(Debug, Clone)]
pub struct Mode {
    pub name: String,
    pub width: u16,
    pub height: u16,
    pub vrefresh: u32,
    pub preferred: bool,
}

pub fn pick_mode(modes: &[Mode]) -> Option<&Mode> {
    // Optional override: YSERVER_MODE=WxH (e.g. "1024x768") wins over the
    // kernel-reported PREFERRED mode. Useful when virtio-gpu's EDID hint
    // is ignored and the driver advertises 640x480 as preferred. Refresh
    // is matched best-effort: 60 Hz first, then any rate.
    if let Ok(spec) = std::env::var("YSERVER_MODE")
        && let Some((w, h)) = parse_mode_spec(&spec)
    {
        if let Some(m) = modes
            .iter()
            .find(|m| m.width == w && m.height == h && m.vrefresh == 60)
        {
            return Some(m);
        }
        if let Some(m) = modes.iter().find(|m| m.width == w && m.height == h) {
            return Some(m);
        }
        log::warn!(
            "YSERVER_MODE={spec} not advertised by the connector; falling back to preferred mode"
        );
    }
    if let Some(m) = modes.iter().find(|m| m.preferred) {
        return Some(m);
    }
    if let Some(m) = modes
        .iter()
        .find(|m| m.width == 1024 && m.height == 768 && m.vrefresh == 60)
    {
        return Some(m);
    }
    modes.first()
}

fn parse_mode_spec(spec: &str) -> Option<(u16, u16)> {
    let (w, h) = spec.split_once('x')?;
    let w: u16 = w.trim().parse().ok()?;
    let h: u16 = h.trim().parse().ok()?;
    Some((w, h))
}

fn local_mode_from(m: &DrmMode) -> Mode {
    let (w, h) = m.size();
    Mode {
        name: m.name().to_string_lossy().into_owned(),
        width: w,
        height: h,
        vrefresh: m.vrefresh(),
        preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED),
    }
}

#[derive(Debug)]
pub struct Output {
    pub connector: connector::Handle,
    pub connector_name: String,
    pub crtc: crtc::Handle,
    pub plane: plane::Handle,
    pub mode: DrmMode,
    pub picked: Mode,
    pub plane_fb_id_prop: property::Handle,
    pub plane_crtc_id_prop: property::Handle,
}

pub fn discover_output(device: &Device) -> io::Result<Output> {
    let resources = device.resource_handles()?;

    let mut connected: Option<connector::Info> = None;
    for &handle in resources.connectors() {
        let info = device.get_connector(handle, false)?;
        if info.state() == connector::State::Connected && !info.modes().is_empty() {
            connected = Some(info);
            break;
        }
    }
    let connector_info = connected.ok_or_else(|| {
        io::Error::other(
            "no connected output — vng with --graphics required for modeset path; \
             headless mode does not exercise this",
        )
    })?;
    let connector_name = format!("{connector_info}");

    let local_modes: Vec<Mode> = connector_info.modes().iter().map(local_mode_from).collect();
    let picked = pick_mode(&local_modes)
        .ok_or_else(|| {
            io::Error::other(format!(
                "connector {connector_name} reports no usable modes",
            ))
        })?
        .clone();
    let picked_idx = local_modes
        .iter()
        .position(|m| {
            m.name == picked.name
                && m.width == picked.width
                && m.height == picked.height
                && m.vrefresh == picked.vrefresh
        })
        .expect("picked mode is from local_modes");
    let drm_mode = connector_info.modes()[picked_idx];

    let encoder_handle = connector_info
        .current_encoder()
        .or_else(|| connector_info.encoders().first().copied())
        .ok_or_else(|| {
            io::Error::other(format!("connector {connector_name} has no usable encoder",))
        })?;
    let encoder = device.get_encoder(encoder_handle)?;
    let crtc = encoder
        .crtc()
        .or_else(|| {
            resources
                .filter_crtcs(encoder.possible_crtcs())
                .into_iter()
                .next()
        })
        .ok_or_else(|| {
            io::Error::other(format!(
                "encoder for connector {connector_name} has no possible CRTC",
            ))
        })?;

    let plane = pick_primary_plane(device, &resources, crtc)?;
    let plane_props_map = PropMap::for_object(device, plane)?;
    let plane_fb_id_prop = plane_props_map.id("FB_ID")?;
    let plane_crtc_id_prop = plane_props_map.id("CRTC_ID")?;

    log::info!(
        "yserver: connector={connector_name} crtc={crtc:?} plane={plane:?} \
         mode={} ({}x{}@{}{})",
        picked.name,
        picked.width,
        picked.height,
        picked.vrefresh,
        if picked.preferred { ", preferred" } else { "" }
    );

    Ok(Output {
        connector: connector_info.handle(),
        connector_name,
        crtc,
        plane,
        mode: drm_mode,
        picked,
        plane_fb_id_prop,
        plane_crtc_id_prop,
    })
}

fn pick_primary_plane(
    device: &Device,
    resources: &drm::control::ResourceHandles,
    crtc: crtc::Handle,
) -> io::Result<plane::Handle> {
    for handle in device.plane_handles()? {
        let info = device.get_plane(handle)?;
        if !resources
            .filter_crtcs(info.possible_crtcs())
            .contains(&crtc)
        {
            continue;
        }
        let props = device.get_properties(handle)?;
        let map = props.as_hashmap(device)?;
        let Some(type_info) = map.get("type") else {
            continue;
        };
        let raw = props
            .iter()
            .find(|(h, _)| **h == type_info.handle())
            .map(|(_, v)| *v)
            .unwrap_or(0);
        if raw == PlaneType::Primary as u64 {
            return Ok(handle);
        }
    }
    Err(io::Error::other(format!(
        "no primary plane available for CRTC {crtc:?}",
    )))
}

pub fn dump_properties(device: &Device, output: &Output) -> io::Result<()> {
    log::debug!("=== connector {} properties ===", output.connector_name);
    dump_object_properties(device, output.connector)?;
    log::debug!("=== crtc {:?} properties ===", output.crtc);
    dump_object_properties(device, output.crtc)?;
    log::debug!("=== plane {:?} properties ===", output.plane);
    dump_object_properties(device, output.plane)?;
    Ok(())
}

fn dump_object_properties<H>(device: &Device, handle: H) -> io::Result<()>
where
    H: drm::control::ResourceHandle,
{
    let props = device.get_properties(handle)?;
    for (prop_handle, raw_value) in &props {
        let info = device.get_property(*prop_handle)?;
        log::debug!(
            "  {} = 0x{:x} ({:?})",
            info.name().to_string_lossy(),
            raw_value,
            info.value_type()
        );
    }
    Ok(())
}

pub(crate) struct PropMap {
    handles: HashMap<String, property::Info>,
}

impl PropMap {
    pub(crate) fn for_object<H>(device: &Device, handle: H) -> io::Result<Self>
    where
        H: drm::control::ResourceHandle,
    {
        let props = device.get_properties(handle)?;
        Ok(Self {
            handles: props.as_hashmap(device)?,
        })
    }

    pub(crate) fn id(&self, name: &str) -> io::Result<property::Handle> {
        self.handles
            .get(name)
            .map(|info| info.handle())
            .ok_or_else(|| io::Error::other(format!("property {name:?} not exposed")))
    }
}

pub fn disable_output(device: &Device, output: &Output) -> io::Result<()> {
    let connector_props = PropMap::for_object(device, output.connector)?;
    let crtc_props = PropMap::for_object(device, output.crtc)?;

    let mut req = AtomicModeReq::new();
    req.add_raw_property(output.plane.into(), output.plane_fb_id_prop, 0);
    req.add_raw_property(output.plane.into(), output.plane_crtc_id_prop, 0);
    req.add_raw_property(output.crtc.into(), crtc_props.id("ACTIVE")?, 0);
    req.add_raw_property(output.crtc.into(), crtc_props.id("MODE_ID")?, 0);
    req.add_raw_property(output.connector.into(), connector_props.id("CRTC_ID")?, 0);

    device
        .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("disable_output atomic commit rejected: {err}"),
            )
        })
}

pub fn commit_modeset(
    device: &Device,
    output: &Output,
    fb_id: framebuffer::Handle,
) -> io::Result<()> {
    let connector_props = PropMap::for_object(device, output.connector)?;
    let crtc_props = PropMap::for_object(device, output.crtc)?;
    let plane_props = PropMap::for_object(device, output.plane)?;

    let mode_blob = device.create_property_blob(&output.mode)?;
    let mode_blob_raw: u64 = mode_blob.into();

    let crtc_id_raw: u32 = output.crtc.into();
    let plane_crtc_raw: u32 = output.crtc.into();
    let fb_id_raw: u32 = fb_id.into();
    let (mode_w, mode_h) = output.mode.size();
    let src_w = u64::from(mode_w) << 16;
    let src_h = u64::from(mode_h) << 16;

    let mut req = AtomicModeReq::new();
    req.add_raw_property(
        output.connector.into(),
        connector_props.id("CRTC_ID")?,
        u64::from(crtc_id_raw),
    );
    req.add_raw_property(output.crtc.into(), crtc_props.id("MODE_ID")?, mode_blob_raw);
    req.add_raw_property(output.crtc.into(), crtc_props.id("ACTIVE")?, 1);
    req.add_raw_property(
        output.plane.into(),
        plane_props.id("FB_ID")?,
        u64::from(fb_id_raw),
    );
    req.add_raw_property(
        output.plane.into(),
        plane_props.id("CRTC_ID")?,
        u64::from(plane_crtc_raw),
    );
    req.add_raw_property(output.plane.into(), plane_props.id("SRC_X")?, 0);
    req.add_raw_property(output.plane.into(), plane_props.id("SRC_Y")?, 0);
    req.add_raw_property(output.plane.into(), plane_props.id("SRC_W")?, src_w);
    req.add_raw_property(output.plane.into(), plane_props.id("SRC_H")?, src_h);
    req.add_raw_property(output.plane.into(), plane_props.id("CRTC_X")?, 0);
    req.add_raw_property(output.plane.into(), plane_props.id("CRTC_Y")?, 0);
    req.add_raw_property(
        output.plane.into(),
        plane_props.id("CRTC_W")?,
        u64::from(mode_w),
    );
    req.add_raw_property(
        output.plane.into(),
        plane_props.id("CRTC_H")?,
        u64::from(mode_h),
    );

    let result = device.atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req);
    let _ = device.destroy_property_blob(mode_blob_raw);
    result.map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "atomic modeset commit rejected (mode {}, {}x{}): {err}",
                output.picked.name, output.picked.width, output.picked.height
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(name: &str, w: u16, h: u16, refresh: u32, preferred: bool) -> Mode {
        Mode {
            name: name.into(),
            width: w,
            height: h,
            vrefresh: refresh,
            preferred,
        }
    }

    #[test]
    fn picks_preferred_when_present() {
        let modes = vec![
            mode("800x600", 800, 600, 60, false),
            mode("1024x768", 1024, 768, 60, true),
            mode("1920x1080", 1920, 1080, 60, false),
        ];
        let picked = pick_mode(&modes).unwrap();
        assert_eq!(picked.name, "1024x768");
    }

    #[test]
    fn falls_back_to_1024x768_60_when_no_preferred() {
        let modes = vec![
            mode("800x600", 800, 600, 60, false),
            mode("1024x768", 1024, 768, 60, false),
            mode("1920x1080", 1920, 1080, 60, false),
        ];
        let picked = pick_mode(&modes).unwrap();
        assert_eq!(picked.name, "1024x768");
    }

    #[test]
    fn falls_back_to_first_when_no_preferred_and_no_1024x768() {
        let modes = vec![
            mode("800x600", 800, 600, 60, false),
            mode("1920x1080", 1920, 1080, 60, false),
        ];
        let picked = pick_mode(&modes).unwrap();
        assert_eq!(picked.name, "800x600");
    }

    #[test]
    fn empty_list_returns_none() {
        assert!(pick_mode(&[]).is_none());
    }
}
