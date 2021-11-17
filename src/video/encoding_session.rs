use std::sync::Arc;

use windows::{
    runtime::Result,
    Foundation::TimeSpan,
    Graphics::{
        Capture::{GraphicsCaptureItem, GraphicsCaptureSession},
        SizeInt32,
    },
    Storage::Streams::IRandomAccessStream,
    Win32::{
        Foundation::PWSTR,
        Graphics::{
            Direct3D11::{
                ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
                D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX,
                D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            },
            Dxgi::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC},
        },
        Media::MediaFoundation::{
            IMFMediaType, IMFSample, IMFSinkWriter, MFCreateAttributes,
            MFCreateMFByteStreamOnStreamEx, MFCreateSinkWriterFromURL,
        },
    },
};

use crate::{
    capture::{CaptureFrame, CaptureFrameWait},
    d3d::get_d3d_interface_from_object,
};

use super::{
    encoder::{VideoEncoder, VideoEncoderInputSample},
    encoder_device::VideoEncoderDevice,
    processor::VideoProcessor,
};

pub struct VideoEncodingSession {
    video_encoder: VideoEncoder,
    capture_session: GraphicsCaptureSession,
    sample_writer: Arc<SampleWriter>,
}

struct SampleGenerator {
    d3d_device: ID3D11Device,
    d3d_context: ID3D11DeviceContext,

    video_processor: VideoProcessor,
    compose_texture: ID3D11Texture2D,
    render_target_view: ID3D11RenderTargetView,

    frame_wait: CaptureFrameWait,

    seen_first_time_stamp: bool,
    first_timestamp: TimeSpan,
}

struct SampleWriter {
    _stream: IRandomAccessStream,
    sink_writer: IMFSinkWriter,
    sink_writer_stream_index: u32,
}

impl VideoEncodingSession {
    pub fn new(
        d3d_device: ID3D11Device,
        item: GraphicsCaptureItem,
        encoder_device: &VideoEncoderDevice,
        resolution: SizeInt32,
        bit_rate: u32,
        frame_rate: u32,
        stream: IRandomAccessStream,
    ) -> Result<Self> {
        let item_size = item.Size()?;
        let input_size = ensure_even_size(item_size);
        let output_size = ensure_even_size(resolution);

        let mut video_encoder = VideoEncoder::new(
            encoder_device,
            d3d_device.clone(),
            output_size,
            output_size,
            bit_rate,
            frame_rate,
        )?;
        let output_type = video_encoder.output_type().clone();

        let mut sample_generator = SampleGenerator::new(d3d_device, item, input_size, output_size)?;
        let capture_session = sample_generator.capture_session().clone();
        video_encoder.set_sample_requested_callback(
            move || -> Result<Option<VideoEncoderInputSample>> { sample_generator.generate() },
        );

        let sample_writer = Arc::new(SampleWriter::new(stream, &output_type)?);
        video_encoder.set_sample_rendered_callback({
            let sample_writer = sample_writer.clone();
            move |sample| -> Result<()> { sample_writer.write(sample.sample()) }
        });

        Ok(Self {
            video_encoder,
            capture_session,
            sample_writer,
        })
    }

    pub fn start(&mut self) -> Result<()> {
        self.sample_writer.start()?;
        self.capture_session.StartCapture()?;
        assert!(self.video_encoder.try_start()?);
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        self.video_encoder.stop()?;
        self.sample_writer.stop()?;
        Ok(())
    }
}

unsafe impl Send for SampleGenerator {}
impl SampleGenerator {
    pub fn new(
        d3d_device: ID3D11Device,
        item: GraphicsCaptureItem,
        input_size: SizeInt32,
        output_size: SizeInt32,
    ) -> Result<Self> {
        let d3d_context = {
            let mut d3d_context = None;
            unsafe { d3d_device.GetImmediateContext(&mut d3d_context) };
            d3d_context.unwrap()
        };

        let video_processor = VideoProcessor::new(
            d3d_device.clone(),
            DXGI_FORMAT_B8G8R8A8_UNORM,
            input_size,
            DXGI_FORMAT_NV12,
            output_size,
        )?;

        let texture_desc = D3D11_TEXTURE2D_DESC {
            Width: input_size.Width as u32,
            Height: input_size.Height as u32,
            ArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                ..Default::default()
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
            ..Default::default()
        };
        let compose_texture =
            unsafe { d3d_device.CreateTexture2D(&texture_desc, std::ptr::null())? };
        let render_target_view =
            unsafe { d3d_device.CreateRenderTargetView(&compose_texture, std::ptr::null())? };

        let frame_wait = CaptureFrameWait::new(d3d_device.clone(), item, input_size)?;

        Ok(Self {
            d3d_device,
            d3d_context,

            video_processor,
            compose_texture,
            render_target_view,

            frame_wait,

            seen_first_time_stamp: false,
            first_timestamp: TimeSpan::default(),
        })
    }

    pub fn capture_session(&self) -> &GraphicsCaptureSession {
        self.frame_wait.session()
    }

    pub fn generate(&mut self) -> Result<Option<VideoEncoderInputSample>> {
        if let Some(frame) = self.frame_wait.try_get_next_frame()? {
            let result = self.generate_from_frame(&frame);
            match result {
                Ok(sample) => Ok(Some(sample)),
                Err(error) => {
                    eprintln!(
                        "Error during input sample generation: {:?} - {}",
                        error.code(),
                        error.message()
                    );
                    self.stop_capture()?;
                    Ok(None)
                }
            }
        } else {
            self.stop_capture()?;
            Ok(None)
        }
    }

    fn stop_capture(&mut self) -> Result<()> {
        self.frame_wait.stop_capture()
    }

    fn generate_from_frame(&mut self, frame: &CaptureFrame) -> Result<VideoEncoderInputSample> {
        if !self.seen_first_time_stamp {
            self.first_timestamp = frame.system_relative_time;
            self.seen_first_time_stamp = true;
        }

        let timestamp = TimeSpan {
            Duration: frame.system_relative_time.Duration - self.first_timestamp.Duration,
        };
        let content_size = frame.content_size;
        let frame_texture: ID3D11Texture2D = get_d3d_interface_from_object(&frame.frame_texture)?;
        let desc = unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            frame_texture.GetDesc(&mut desc);
            desc
        };

        // In order to support window resizing, we need to only copy out the part of
        // the buffer that contains the window. If the window is smaller than the buffer,
        // then it's a straight forward copy using the ContentSize. If the window is larger,
        // we need to clamp to the size of the buffer. For simplicity, we always clamp.
        let width = content_size.Width.clamp(0, desc.Width as i32) as u32;
        let height = content_size.Height.clamp(0, desc.Height as i32) as u32;

        let region = D3D11_BOX {
            left: 0,
            right: width,
            top: 0,
            bottom: height,
            back: 1,
            front: 0,
        };

        unsafe {
            self.d3d_context
                .ClearRenderTargetView(&self.render_target_view, CLEAR_COLOR.as_ptr());
            self.d3d_context.CopySubresourceRegion(
                &self.compose_texture,
                0,
                0,
                0,
                0,
                &frame_texture,
                0,
                &region,
            );

            // Process our back buffer
            self.video_processor
                .process_texture(&self.compose_texture)?;

            // Get our NV12 texture
            let video_output_texture = self.video_processor.output_texture();

            // Make a copy for the sample
            let desc = {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                video_output_texture.GetDesc(&mut desc);
                desc
            };
            let sample_texture = self.d3d_device.CreateTexture2D(&desc, std::ptr::null())?;
            self.d3d_context
                .CopyResource(&sample_texture, video_output_texture);

            Ok(VideoEncoderInputSample::new(timestamp, sample_texture))
        }
    }
}

unsafe impl Send for SampleWriter {}
unsafe impl Sync for SampleWriter {}
impl SampleWriter {
    pub fn new(stream: IRandomAccessStream, output_type: &IMFMediaType) -> Result<Self> {
        let empty_attributes = unsafe {
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 0)?;
            attributes.unwrap()
        };
        let sink_writer = unsafe {
            let byte_stream = MFCreateMFByteStreamOnStreamEx(&stream)?;
            let mut url: Vec<u16> = ".mp4".encode_utf16().collect();
            url.push(0);
            MFCreateSinkWriterFromURL(PWSTR(url.as_mut_ptr()), byte_stream, &empty_attributes)?
        };
        let sink_writer_stream_index = unsafe { sink_writer.AddStream(output_type)? };
        unsafe {
            sink_writer.SetInputMediaType(
                sink_writer_stream_index,
                output_type,
                &empty_attributes,
            )?
        };

        Ok(Self {
            _stream: stream,
            sink_writer,
            sink_writer_stream_index,
        })
    }

    pub fn start(&self) -> Result<()> {
        unsafe { self.sink_writer.BeginWriting() }
    }

    pub fn stop(&self) -> Result<()> {
        unsafe { self.sink_writer.Finalize() }
    }

    pub fn write(&self, sample: &IMFSample) -> Result<()> {
        unsafe {
            self.sink_writer
                .WriteSample(self.sink_writer_stream_index, sample)
        }
    }
}

const CLEAR_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

fn ensure_even(value: i32) -> i32 {
    if value % 2 == 0 {
        value
    } else {
        value + 1
    }
}

fn ensure_even_size(size: SizeInt32) -> SizeInt32 {
    SizeInt32 {
        Width: ensure_even(size.Width),
        Height: ensure_even(size.Height),
    }
}
