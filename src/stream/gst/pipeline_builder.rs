use crate::{
    stream::types::VideoCaptureConfiguration,
    video::{
        types::{VideoEncodeType, VideoSourceType},
        video_source_gst::VideoSourceGstType,
        video_source_local::VideoSourceLocalType,
    },
    video_stream::types::VideoAndStreamInformation,
};
use simple_error::{simple_error, SimpleResult};
use tracing::*;

#[derive(Clone, Debug, Default)]
pub struct Pipeline {
    pub description: String,
}

impl Pipeline {
    pub fn new(video_and_stream_information: &VideoAndStreamInformation) -> SimpleResult<Self> {
        let source = Pipeline::build_pipeline_source(video_and_stream_information)?;
        let transcode = Pipeline::build_pipeline_transcode(video_and_stream_information)?;
        let payload = Pipeline::build_pipeline_payload(video_and_stream_information)?;
        let sink = Pipeline::build_pipeline_sink(video_and_stream_information)?;

        let description = format!("{source}{transcode}{payload}{sink}");

        info!("New pipeline built: {description:#?}");

        Ok(Pipeline { description })
    }

    fn build_capability_string(
        video_and_stream_information: &VideoAndStreamInformation,
    ) -> SimpleResult<String> {
        let configuration =
            Pipeline::get_video_capture_configuration(&video_and_stream_information)?;

        let format = match video_and_stream_information.video_source {
            // Fakes (videotestsrc) are only "video/x-raw" or "video/x-bayer",
            // and to be able to encode it, we need to define an available
            // format for both its src the next element's sink pad.
            // We are choosing "UYVY" because it is compatible by the
            // application-rtp template capabilities.
            // For more information: https://gstreamer.freedesktop.org/documentation/additional/design/mediatype-video-raw.html?gi-language=c#formats
            VideoSourceType::Gst(_) => "video/x-raw,format=UYVY",
            _ => match &configuration.encode {
                VideoEncodeType::H264 => "video/x-h264",
                VideoEncodeType::YUYV => "video/x-raw,format=YUY2",
                VideoEncodeType::MJPG => "image/jpeg",
                video_encode_type => {
                    return Err(simple_error!(format!(
                        "Unsupported VideoEncodeType: {video_encode_type:#?}",
                    )))
                }
            },
        };
        let pipeline_capability = format!(
            concat!(
                "{format},width={width},height={height},framerate={interval_denominator}/{interval_numerator}",
            ),
            format = format,
            width = configuration.width,
            height = configuration.height,
            interval_denominator = configuration.frame_interval.denominator,
            interval_numerator = configuration.frame_interval.numerator,
        );
        Ok(pipeline_capability)
    }

    fn build_pipeline_source(
        video_and_stream_information: &VideoAndStreamInformation,
    ) -> SimpleResult<String> {
        let pipeline_source = match &video_and_stream_information.video_source {
            VideoSourceType::Gst(gst_source) => match &gst_source.source {
                VideoSourceGstType::Fake(pattern) => format!("videotestsrc pattern={pattern}"),
                VideoSourceGstType::Local(_) => {
                    return Err(simple_error!(format!(
                        "Unsupported GST source endpoint: {gst_source:#?}",
                    )));
                }
            },
            VideoSourceType::Local(local_device) => match &local_device.typ {
                VideoSourceLocalType::Usb(_) | VideoSourceLocalType::LegacyRpiCam(_) => {
                    format!("v4l2src device={}", &local_device.device_path)
                }
                typ => {
                    return Err(simple_error!(format!(
                        "Unsuported VideoSourceLocal: {typ:#?}."
                    )))
                }
            },
            video_source_type => {
                return Err(simple_error!(format!(
                    "Unsupported VideoSourceType: {video_source_type:#?}.",
                )));
            }
        };

        let capability = Pipeline::build_capability_string(&video_and_stream_information)?;
        Ok(format!("{pipeline_source} ! {capability}"))
    }

    fn build_pipeline_transcode(
        video_and_stream_information: &VideoAndStreamInformation,
    ) -> SimpleResult<String> {
        let configuration =
            Pipeline::get_video_capture_configuration(video_and_stream_information)?;

        let pipeline_transcode = match &video_and_stream_information.video_source {
            VideoSourceType::Gst(_) => match configuration.encode {
                // Fake sources are video/x-raw, so we need to encode it to
                // have h264 or mjpg.
                VideoEncodeType::H264 => concat!(
                    " ! videoconvert",
                    " ! x264enc bitrate=5000",
                    " ! video/x-h264,profile=baseline",
                ),
                VideoEncodeType::MJPG => concat!(" ! jpegenc",),
                _ => "",
            },
            VideoSourceType::Local(_) => match configuration.encode {
                // Because application-rtp templates doesn't accept "YUY2", we
                // need to transcode it. We are arbitrarily chosing the closest
                // format available ("UYVY").
                VideoEncodeType::YUYV => concat!(" ! videoconvert", " ! video/x-raw,format=UYVY",),
                _ => "",
            },
            video_source_type => {
                return Err(simple_error!(format!(
                    "Unsupported VideoSourceType: {video_source_type:#?}.",
                )));
            }
        };
        Ok(pipeline_transcode.to_string())
    }

    fn build_pipeline_payload(
        video_and_stream_information: &VideoAndStreamInformation,
    ) -> SimpleResult<String> {
        let configuration =
            Pipeline::get_video_capture_configuration(&video_and_stream_information)?;

        let pipeline_payload = match &configuration.encode {
            // Here we are naming the payloader as pay0 because the rtsp server
            // expects this specific name, and having a name doesn't hurt any
            // other endpoint type.
            VideoEncodeType::H264 => concat!(
                " ! h264parse",
                " ! queue",
                " ! rtph264pay name=pay0 config-interval=10 pt=96",
            ),
            VideoEncodeType::YUYV => concat!(
                " ! rtpvrawpay name=pay0",
                // Again, as we are always using the "UYVY" format for raw
                // application/rtp payloads, "YCbCr-4:2:2" will always be
                // the right one to pick.
                " ! application/x-rtp,payload=96,sampling=YCbCr-4:2:2",
            ),
            VideoEncodeType::MJPG => " ! rtpjpegpay name=pay0 pt=96",
            video_encode_type => {
                return Err(simple_error!(format!(
                    "Unsupported VideoEncodeType: {video_encode_type:#?}"
                )))
            }
        };
        Ok(pipeline_payload.to_string())
    }

    fn build_pipeline_sink(
        video_and_stream_information: &VideoAndStreamInformation,
    ) -> SimpleResult<String> {
        let endpoints = &video_and_stream_information.stream_information.endpoints;
        let pipeline_sink = match endpoints[0].scheme() {
            "udp" => {
                let clients = endpoints
                    .iter()
                    .map(|endpoint| {
                        format!("{}:{}", endpoint.host().unwrap(), endpoint.port().unwrap())
                    })
                    .collect::<Vec<String>>()
                    .join(",");
                format!(" ! multiudpsink clients={clients}")
            }
            _ => "".to_string(),
        };
        Ok(pipeline_sink)
    }

    fn get_video_capture_configuration(
        video_and_stream_information: &VideoAndStreamInformation,
    ) -> SimpleResult<&VideoCaptureConfiguration> {
        let configuration = match &video_and_stream_information
            .stream_information
            .configuration
        {
            crate::stream::types::CaptureConfiguration::VIDEO(configuration) => configuration,
            crate::stream::types::CaptureConfiguration::REDIRECT(_) => {
                return Err(simple_error!(
                    "Error: Cannot create a pipeline from a REDIRECT source!"
                ))
            }
        };
        Ok(configuration)
    }
}
