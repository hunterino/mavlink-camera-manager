use std::cmp::max;

use super::types::*;
use super::{
    video_source,
    video_source::{VideoSource, VideoSourceAvailable},
};
use paperclip::actix::Apiv2Schema;
use regex::Regex;
use serde::{Deserialize, Serialize};
use v4l::prelude::*;
use v4l::video::Capture;

use tracing::*;

//TODO: Move to types
#[derive(Apiv2Schema, Clone, Debug, PartialEq, Deserialize, Serialize)]
pub enum VideoSourceLocalType {
    Unknown(String),
    Usb(String),
    LegacyRpiCam(String),
}

#[derive(Apiv2Schema, Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VideoSourceLocal {
    pub name: String,
    pub device_path: String,
    #[serde(rename = "type")]
    pub typ: VideoSourceLocalType,
}

impl VideoSourceLocalType {
    // For PCI:
    // https://wiki.xenproject.org/wiki/Bus:Device.Function_(BDF)_Notation
    // description should follow: <domain>:<bus>:<device>.<first_function>-<last_function>
    // E.g: usb-0000:08:00.3-1, where domain, bus and device are hexadecimal
    // `first_function` describes the usb HUB and `last_function` describes the USB port of that HUB
    //
    // For devices that does not have PCI, the information will come with
    // the following description: usb-<unknown>z.<usb-usb_hub>.<usb_port>
    // E.g: usb-3f980000.usb-1.4, where unknown is hexadecimal
    // `udevadm info` can also provide information about the camera
    //
    // https://www.kernel.org/doc/html/v4.9/media/uapi/v4l/vidioc-querycap.html#:~:text=__u8-,bus_info,-%5B32%5D

    pub fn from_str(description: &str) -> Self {
        if let Some(result) = VideoSourceLocalType::usb_from_str(description) {
            return result;
        }

        if let Some(result) = VideoSourceLocalType::v4l2_from_str(description) {
            return result;
        }

        let msg = format!("Unable to identify the local camera connection type, please report the problem: {description}");
        if description == "platform:bcm2835-isp" {
            // Filter out the log for this particular device, because regarding to Raspberry Pis, it will always be there and we will never use it.
            trace!(msg);
        } else {
            warn!(msg);
        }
        return VideoSourceLocalType::Unknown(description.into());
    }

    fn usb_from_str(description: &str) -> Option<Self> {
        let regex =
            Regex::new(r"usb-(?P<interface>(([0-9a-fA-F]{2}){1,2}:?){4})?\.(usb-)?(?P<device>.*)")
                .unwrap();
        if regex.is_match(description) {
            return Some(VideoSourceLocalType::Usb(description.into()));
        }
        return None;
    }

    fn v4l2_from_str(description: &str) -> Option<Self> {
        let regex = Regex::new(r"platform:(?P<device>\S+)-v4l2-[0-9]").unwrap();
        if regex.is_match(description) {
            return Some(VideoSourceLocalType::LegacyRpiCam(description.into()));
        }
        return None;
    }
}

impl VideoSourceLocal {
    pub fn update_device(&mut self) -> bool {
        if let VideoSourceLocalType::Usb(our_usb_bus) = &self.typ {
            let cameras = video_source::cameras_available();
            let camera: Option<VideoSourceType> = cameras
                .into_iter()
                .filter(|camera| match camera {
                    VideoSourceType::Local(camera) => match &camera.typ {
                        VideoSourceLocalType::Usb(usb_bus) => *usb_bus == *our_usb_bus,
                        _ => false,
                    },
                    _ => false,
                })
                .next();

            match camera {
                None => {
                    error!("Failed to find camera: {:#?}", self);
                    error!("Camera will be set as invalid.");
                    self.device_path = "".into();
                    return false;
                }
                Some(camera) => {
                    if let VideoSourceType::Local(camera) = camera {
                        if camera.device_path == self.device_path {
                            return true;
                        }

                        info!("Camera path changed.");
                        info!("Previous camera location: {:#?}", self);
                        info!("New camera location: {:#?}", camera);
                        *self = camera.clone();
                        return true;
                    }
                    unreachable!();
                }
            }
        }
        return true;
    }
}

fn convert_v4l_intervals(v4l_intervals: &[v4l::FrameInterval]) -> Vec<FrameInterval> {
    let mut intervals: Vec<FrameInterval> = vec![];

    v4l_intervals
        .iter()
        .for_each(|v4l_interval| match &v4l_interval.interval {
            v4l::frameinterval::FrameIntervalEnum::Discrete(fraction) => {
                intervals.push(FrameInterval {
                    numerator: fraction.numerator,
                    denominator: fraction.denominator,
                })
            }
            v4l::frameinterval::FrameIntervalEnum::Stepwise(stepwise) => {
                // To avoid a having a huge number of numerator/denominators, we
                // arbitrarely set a minimum step of 5 units
                let min_step = 5;
                let numerator_step = max(stepwise.step.numerator, min_step);
                let denominator_step = max(stepwise.step.denominator, min_step);

                let numerators = (0..=stepwise.min.numerator)
                    .step_by(numerator_step as usize)
                    .chain(vec![stepwise.max.numerator])
                    .collect::<Vec<u32>>();
                let denominators = (0..=stepwise.min.denominator)
                    .step_by(denominator_step as usize)
                    .chain(vec![stepwise.max.denominator])
                    .collect::<Vec<u32>>();

                for numerator in &numerators {
                    for denominator in &denominators {
                        intervals.push(FrameInterval {
                            numerator: max(1, *numerator),
                            denominator: max(1, *denominator),
                        });
                    }
                }
            }
        });

    intervals.sort();
    intervals.dedup();
    intervals.reverse();

    intervals
}

impl VideoSource for VideoSourceLocal {
    fn name(&self) -> &String {
        return &self.name;
    }

    fn source_string(&self) -> &str {
        return &self.device_path;
    }

    fn formats(&self) -> Vec<Format> {
        let device = Device::with_path(&self.device_path).unwrap();
        let v4l_formats = device.enum_formats().unwrap_or_default();
        let mut formats = vec![];

        trace!("Checking resolutions for camera: {}", &self.device_path);
        for v4l_format in v4l_formats {
            let mut sizes = vec![];
            let mut errors: Vec<String> = vec![];

            for v4l_framesizes in device.enum_framesizes(v4l_format.fourcc).unwrap() {
                match v4l_framesizes.size {
                    v4l::framesize::FrameSizeEnum::Discrete(v4l_size) => {
                        match &device.enum_frameintervals(
                            v4l_framesizes.fourcc,
                            v4l_size.width,
                            v4l_size.height,
                        ) {
                            Ok(enum_frameintervals) => {
                                let intervals = convert_v4l_intervals(enum_frameintervals);
                                sizes.push(Size {
                                    width: v4l_size.width,
                                    height: v4l_size.height,
                                    intervals: intervals.into(),
                                })
                            }
                            Err(error) => {
                                errors.push(format!(
                                    "encode: {encode:?}, for size: {v4l_size:?}, error: {error:#?}",
                                    encode = v4l_format.fourcc,
                                ));
                            }
                        }
                    }
                    v4l::framesize::FrameSizeEnum::Stepwise(v4l_size) => {
                        let mut std_sizes: Vec<(u32, u32)> = STANDARD_SIZES.to_vec();
                        std_sizes.push((v4l_size.max_width, v4l_size.max_height));

                        std_sizes.iter().for_each(|(width, height)| {
                            match &device.enum_frameintervals(
                                v4l_framesizes.fourcc,
                                *width,
                                *height,
                            ) {
                                Ok(enum_frameintervals) => {
                                    let intervals = convert_v4l_intervals(enum_frameintervals);
                                    sizes.push(Size {
                                        width: *width,
                                        height: *height,
                                        intervals: intervals.into(),
                                    });
                                }
                                Err(error) => {
                                    errors.push(format!(
                                        "encode: {encode:?}, for size: {v4l_size:?}, error: {error:#?}",
                                        encode = v4l_format.fourcc,
                                        v4l_size = (width, height),
                                    ));
                                }
                            };
                        });
                    }
                }
            }

            sizes.sort();
            sizes.dedup();
            sizes.reverse();

            if !errors.is_empty() {
                trace!(
                    "Failed to fetch frameintervals for camera {}: {errors:#?}",
                    &self.device_path
                );
            }

            formats.push(Format {
                encode: VideoEncodeType::from_str(v4l_format.fourcc.str().unwrap()),
                sizes,
            });
        }

        // V4l2 reports unsupported sizes for Raspberry Pi
        // Cameras in Legacy Mode, showing the following:
        // > mmal: mmal_vc_port_enable: failed to enable port vc.ril.video_encode:in:0(OPQV): EINVAL
        // > mmal: mmal_port_enable: failed to enable connected port (vc.ril.video_encode:in:0(OPQV))0x75903be0 (EINVAL)
        // > mmal: mmal_connection_enable: output port couldn't be enabled
        // To prevent it, we are currently constraining it
        // to a max. of 1920 x 1080 px, and a max. 30 FPS.
        if matches!(&self.typ, VideoSourceLocalType::LegacyRpiCam(_)) {
            warn!("To support Raspiberry Pi Cameras in Legacy Camera Mode without bugs, resolution is constrained to 1920 x 1080 @ 30FPS.");
            let max_width = 1920;
            let max_height = 1080;
            let max_fps = 30;
            formats.iter_mut().for_each(|format| {
                format.sizes.iter_mut().for_each(|size| {
                    if size.width > max_width {
                        size.width = max_width;
                    }

                    if size.height > max_height {
                        size.height = max_height;
                    }

                    size.intervals = size
                        .intervals
                        .clone()
                        .into_iter()
                        .filter(|interval| interval.numerator * interval.denominator <= max_fps)
                        .collect();
                });

                format.sizes.dedup();
            });
        }

        formats.sort();
        formats.dedup();

        formats
    }

    fn set_control_by_name(&self, _control_name: &str, _value: i64) -> std::io::Result<()> {
        unimplemented!();
    }

    fn set_control_by_id(&self, control_id: u64, value: i64) -> std::io::Result<()> {
        let control = self
            .controls()
            .into_iter()
            .find(|control| control.id == control_id);

        if control.is_none() {
            let ids: Vec<u64> = self.controls().iter().map(|control| control.id).collect();
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "Control ID '{}' is not valid, options are: {:?}",
                    control_id, ids
                ),
            ));
        }
        let control = control.unwrap();

        //TODO: Add control validation
        let device = Device::with_path(&self.device_path)?;
        //TODO: we should handle value, value64 and string
        match device.set_control(
            control_id as u32,
            v4l::control::Control::Value(value as i32),
        ) {
            ok @ Ok(_) => ok,
            Err(error) => {
                warn!("Failed to set control {:#?}, error: {:#?}", control, error);
                Err(error)
            }
        }
    }

    fn control_value_by_name(&self, _control_name: &str) -> std::io::Result<i64> {
        unimplemented!();
    }

    fn control_value_by_id(&self, control_id: u64) -> std::io::Result<i64> {
        let device = Device::with_path(&self.device_path)?;
        let value = device.control(control_id as u32)?;
        match value {
            v4l::control::Control::String(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "String control type is not supported.",
                ));
            }
            v4l::control::Control::Value(value) => return Ok(value as i64),
            v4l::control::Control::Value64(value) => return Ok(value),
        }
    }

    fn controls(&self) -> Vec<Control> {
        //TODO: create function to encapsulate device
        let device = Device::with_path(&self.device_path).unwrap();
        let v4l_controls = device.query_controls().unwrap_or_default();

        let mut controls: Vec<Control> = vec![];
        for v4l_control in v4l_controls {
            let mut control = Control {
                name: v4l_control.name,
                id: v4l_control.id as u64,
                state: ControlState {
                    is_disabled: v4l_control.flags.contains(v4l::control::Flags::DISABLED),
                    is_inactive: v4l_control.flags.contains(v4l::control::Flags::INACTIVE),
                },
                ..Default::default()
            };

            if matches!(v4l_control.typ, v4l::control::Type::CtrlClass) {
                // CtrlClass is not a control, so we are skipping it to avoid any access to it, as it will raise an
                // IO error #13: Permission Denied. To better understand, look for 'V4L2_CTRL_TYPE_CTRL_CLASS' on
                // this doc: https://www.kernel.org/doc/html/v5.5/media/uapi/v4l/vidioc-queryctrl.html#c.v4l2_ctrl_type
                continue;
            }

            let value = self.control_value_by_id(v4l_control.id as u64);
            if let Err(error) = value {
                error!(
                    "Failed to get control '{} ({})' from device {}: {error}",
                    control.name, control.id, self.device_path
                );
                continue;
            }
            let value = value.unwrap();
            let default = v4l_control.default;

            match v4l_control.typ {
                v4l::control::Type::Boolean => {
                    control.cpp_type = "bool".to_string();
                    control.configuration = ControlType::Bool(ControlBool { default, value });
                    controls.push(control);
                }
                v4l::control::Type::Integer | v4l::control::Type::Integer64 => {
                    control.cpp_type = "int64".to_string();
                    control.configuration = ControlType::Slider(ControlSlider {
                        default,
                        value,
                        step: v4l_control.step,
                        max: v4l_control.maximum,
                        min: v4l_control.minimum,
                    });
                    controls.push(control);
                }
                v4l::control::Type::Menu | v4l::control::Type::IntegerMenu => {
                    control.cpp_type = "int32".to_string();
                    if let Some(items) = v4l_control.items {
                        let options = items
                            .iter()
                            .map(|(value, name)| ControlOption {
                                name: match name {
                                    v4l::control::MenuItem::Name(name) => name.clone(),
                                    v4l::control::MenuItem::Value(name) => name.to_string(),
                                },
                                value: *value as i64,
                            })
                            .collect();
                        control.configuration = ControlType::Menu(ControlMenu {
                            default,
                            value,
                            options,
                        });
                        controls.push(control);
                    }
                }
                _ => continue,
            };
        }
        return controls;
    }

    fn is_valid(&self) -> bool {
        return !self.device_path.is_empty();
    }

    fn is_shareable(&self) -> bool {
        return false;
    }
}

impl VideoSourceAvailable for VideoSourceLocal {
    fn cameras_available() -> Vec<VideoSourceType> {
        let cameras_path: Vec<String> = std::fs::read_dir("/dev/")
            .unwrap()
            .map(|f| String::from(f.unwrap().path().to_str().unwrap()))
            .filter(|f| f.starts_with("/dev/video"))
            .collect();

        let mut cameras: Vec<VideoSourceType> = vec![];
        for camera_path in &cameras_path {
            let camera = Device::with_path(camera_path).unwrap();
            let caps = camera.query_caps();

            if let Err(error) = caps {
                debug!(
                    "Failed to capture caps for device: {} {:#?}",
                    camera_path, error
                );
                continue;
            }
            let caps = caps.unwrap();

            if let Err(error) = camera.format() {
                if error.kind() != std::io::ErrorKind::InvalidInput {
                    debug!(
                        "Failed to capture formats for device: {}\nError: {:#?}",
                        camera_path, error
                    );
                }
                continue;
            }

            let source = VideoSourceLocal {
                name: caps.card,
                device_path: camera_path.clone(),
                typ: VideoSourceLocalType::from_str(&caps.bus),
            };
            cameras.push(VideoSourceType::Local(source));
        }

        return cameras;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]

    fn bus_decode() {
        let descriptions = vec![
            (
                // Normal desktop
                VideoSourceLocalType::Usb("usb-0000:08:00.3-1".into()),
                "usb-0000:08:00.3-1",
            ),
            (
                // Normal desktop with additional hubs
                VideoSourceLocalType::Usb("usb-0000:08:00.3-2.1".into()),
                "usb-0000:08:00.3-2.1",
            ),
            (
                // Provided by the raspberry pi with a USB camera
                VideoSourceLocalType::Usb("usb-3f980000.usb-1.4".into()),
                "usb-3f980000.usb-1.4",
            ),
            (
                // Provided by the raspberry pi with a Raspberry Pi camera when in to use legacy camera mode
                VideoSourceLocalType::LegacyRpiCam("platform:bcm2835-v4l2-0".into()),
                "platform:bcm2835-v4l2-0",
            ),
            (
                // Sanity test
                VideoSourceLocalType::Unknown("potato".into()),
                "potato",
            ),
        ];

        for description in descriptions {
            assert_eq!(description.0, VideoSourceLocalType::from_str(description.1));
        }
    }

    #[allow(dead_code)]
    fn simple_test() {
        for camera in VideoSourceLocal::cameras_available() {
            if let VideoSourceType::Local(camera) = camera {
                println!("Camera: {:#?}", camera);
                println!("Resolutions: {:#?}", camera.formats());
                println!("Controls: {:#?}", camera.controls());
            }
        }
    }
}
