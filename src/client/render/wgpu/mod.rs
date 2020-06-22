// mod atlas;
mod alias;
mod brush;
mod error;
mod glyph;
mod palette;
mod quad;
mod sprite;
mod uniform;
mod warp;

pub use error::{RenderError, RenderErrorKind};
pub use palette::Palette;

use std::{
    borrow::Cow,
    cell::{Ref, RefCell, RefMut},
    mem::size_of,
    rc::Rc,
};

use crate::{
    client::{
        render::wgpu::{
            alias::AliasRenderer,
            brush::{BrushRenderer, BrushRendererBuilder},
            glyph::{GlyphRenderer, GlyphRendererCommand, GlyphUniforms},
            sprite::SpriteRenderer,
            uniform::{DynamicUniformBuffer, DynamicUniformBufferBlock},
        },
        ClientEntity,
    },
    common::{
        engine,
        model::{Model, ModelKind},
        sprite::SpriteKind,
        util::{any_as_bytes, any_slice_as_bytes},
        vfs::Vfs,
        wad::{QPic, Wad},
    },
};

use cgmath::{Deg, Euler, Matrix4, SquareMatrix, Vector3, Vector4, Zero};
use chrono::Duration;
use failure::{Error, Fail};
use shaderc::{CompileOptions, Compiler};
use strum::IntoEnumIterator;

pub const COLOR_ATTACHMENT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;
const DEPTH_ATTACHMENT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const DIFFUSE_TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const FULLBRIGHT_TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;
const LIGHTMAP_TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

pub fn screen_space_vertex_transform(
    display_w: u32,
    display_h: u32,
    quad_w: u32,
    quad_h: u32,
    pos_x: i32,
    pos_y: i32,
) -> Matrix4<f32> {
    // rescale from [0, DISPLAY_*] to [-1, 1] (NDC)
    let ndc_x = (pos_x * 2 - display_w as i32) as f32 / display_w as f32;
    let ndc_y = (pos_y * 2 - display_h as i32) as f32 / display_h as f32;

    let scale_x = (quad_w * 2) as f32 / display_w as f32;
    let scale_y = (quad_h * 2) as f32 / display_h as f32;

    Matrix4::from_translation([ndc_x, ndc_y, 0.0].into())
        * Matrix4::from_nonuniform_scale(scale_x, scale_y, 1.0)
}

lazy_static! {
    static ref BIND_GROUP_LAYOUT_DESCRIPTOR_BINDINGS: [Vec<wgpu::BindGroupLayoutEntry>; 2] = [
        vec![
            wgpu::BindGroupLayoutEntry::new(
                0,
                wgpu::ShaderStage::all(),
                wgpu::BindingType::UniformBuffer {
                    dynamic: false,
                    min_binding_size: Some(
                        std::num::NonZeroU64::new(size_of::<FrameUniforms>() as u64).unwrap(),
                    ),
                },
            ),
        ],
        vec![
            // transform matrix
            // TODO: move this to push constants once they're exposed in wgpu
            wgpu::BindGroupLayoutEntry::new(
                0,
                wgpu::ShaderStage::VERTEX,
                wgpu::BindingType::UniformBuffer {
                    dynamic: true,
                    min_binding_size: Some(
                        std::num::NonZeroU64::new(size_of::<EntityUniforms>() as u64)
                            .unwrap(),
                    ),
                },
            ),
            // diffuse and fullbright sampler
            wgpu::BindGroupLayoutEntry::new(
                1,
                wgpu::ShaderStage::FRAGMENT,
                wgpu::BindingType::Sampler { comparison: false },
            ),
            // lightmap sampler
            wgpu::BindGroupLayoutEntry::new(
                2,
                wgpu::ShaderStage::FRAGMENT,
                wgpu::BindingType::Sampler { comparison: false },
            ),
        ],
    ];

    static ref BIND_GROUP_LAYOUT_DESCRIPTORS: [wgpu::BindGroupLayoutDescriptor<'static>; 2] = [
        // group 0: updated per-frame
        wgpu::BindGroupLayoutDescriptor {
            label: Some("per-frame bind group"),
            bindings: &BIND_GROUP_LAYOUT_DESCRIPTOR_BINDINGS[0],
        },
        // group 1: updated per-entity
        wgpu::BindGroupLayoutDescriptor {
            label: Some("brush per-entity bind group"),
            bindings: &BIND_GROUP_LAYOUT_DESCRIPTOR_BINDINGS[1],
        },
    ];
}

pub trait Pipeline {
    fn name() -> &'static str;
    fn bind_group_layout_descriptors() -> Vec<wgpu::BindGroupLayoutDescriptor<'static>>;
    fn vertex_shader() -> &'static str;
    fn fragment_shader() -> &'static str;
    fn rasterization_state_descriptor() -> Option<wgpu::RasterizationStateDescriptor>;
    fn primitive_topology() -> wgpu::PrimitiveTopology;
    fn color_state_descriptors() -> Vec<wgpu::ColorStateDescriptor>;
    fn depth_stencil_state_descriptor() -> Option<wgpu::DepthStencilStateDescriptor>;
    fn vertex_buffer_descriptors() -> Vec<wgpu::VertexBufferDescriptor<'static>>;
}

// bind_group_layout_prefix is a set of bind group layouts to be prefixed onto
// P::BIND_GROUP_LAYOUT_DESCRIPTORS in order to allow layout reuse between pipelines
pub fn create_pipeline<'a, P>(
    device: &wgpu::Device,
    compiler: &mut shaderc::Compiler,
    bind_group_layout_prefix: &[wgpu::BindGroupLayout],
) -> (wgpu::RenderPipeline, Vec<wgpu::BindGroupLayout>)
where
    P: Pipeline,
{
    info!("Creating {} pipeline", P::name());
    let bind_group_layouts = P::bind_group_layout_descriptors()
        .iter()
        .map(|desc| device.create_bind_group_layout(desc))
        .collect::<Vec<_>>();
    info!(
        "{} layouts in prefix | {} specific to pipeline",
        bind_group_layout_prefix.len(),
        bind_group_layouts.len(),
    );

    let pipeline_layout = {
        // add bind group layout prefix
        let layouts: Vec<&wgpu::BindGroupLayout> = bind_group_layout_prefix
            .iter()
            .chain(bind_group_layouts.iter())
            .collect();
        info!("{} layouts total", layouts.len());
        let desc = wgpu::PipelineLayoutDescriptor {
            bind_group_layouts: &layouts,
        };
        device.create_pipeline_layout(&desc)
    };

    let vertex_shader_spirv = compiler
        .compile_into_spirv(
            P::vertex_shader().as_ref(),
            shaderc::ShaderKind::Vertex,
            &format!("{}.vert", P::name()),
            "main",
            None,
        )
        .unwrap();
    let vertex_shader = device.create_shader_module(wgpu::ShaderModuleSource::SpirV(
        vertex_shader_spirv.as_binary(),
    ));
    let fragment_shader_spirv = compiler
        .compile_into_spirv(
            P::fragment_shader().as_ref(),
            shaderc::ShaderKind::Fragment,
            &format!("{}.frag", P::name()),
            "main",
            None,
        )
        .unwrap();
    let fragment_shader = device.create_shader_module(wgpu::ShaderModuleSource::SpirV(
        fragment_shader_spirv.as_binary(),
    ));

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        layout: &pipeline_layout,
        vertex_stage: wgpu::ProgrammableStageDescriptor {
            module: &vertex_shader,
            entry_point: "main",
        },
        fragment_stage: Some(wgpu::ProgrammableStageDescriptor {
            module: &fragment_shader,
            entry_point: "main",
        }),
        rasterization_state: P::rasterization_state_descriptor(),
        primitive_topology: P::primitive_topology(),
        color_states: &P::color_state_descriptors(),
        depth_stencil_state: P::depth_stencil_state_descriptor(),
        vertex_state: wgpu::VertexStateDescriptor {
            index_format: wgpu::IndexFormat::Uint32,
            vertex_buffers: &P::vertex_buffer_descriptors(),
        },
        sample_count: 1,
        sample_mask: !0,
        alpha_to_coverage_enabled: false,
    });

    (pipeline, bind_group_layouts)
}

pub fn create_render_pipeline<'a, I, S>(
    device: &wgpu::Device,
    compiler: &mut shaderc::Compiler,
    name: S,
    bind_group_layouts: I,
    vertex_shader: S,
    fragment_shader: S,
    rasterization_state: Option<wgpu::RasterizationStateDescriptor>,
    primitive_topology: wgpu::PrimitiveTopology,
    color_states: &[wgpu::ColorStateDescriptor],
    depth_stencil_state: Option<wgpu::DepthStencilStateDescriptor>,
    vertex_buffer_descriptors: &[wgpu::VertexBufferDescriptor],
) -> wgpu::RenderPipeline
where
    I: IntoIterator<Item = &'a wgpu::BindGroupLayout>,
    S: AsRef<str>,
{
    let name = name.as_ref();

    let pipeline_layout = {
        let layouts: Vec<&wgpu::BindGroupLayout> = bind_group_layouts
            .into_iter()
            .map(|layout| layout)
            .collect();
        let desc = wgpu::PipelineLayoutDescriptor {
            bind_group_layouts: &layouts,
        };
        device.create_pipeline_layout(&desc)
    };

    let vertex_shader_spirv = compiler
        .compile_into_spirv(
            vertex_shader.as_ref(),
            shaderc::ShaderKind::Vertex,
            &format!("{}.vert", name),
            "main",
            None,
        )
        .unwrap();
    let vertex_shader = device.create_shader_module(wgpu::ShaderModuleSource::SpirV(
        vertex_shader_spirv.as_binary(),
    ));
    let fragment_shader_spirv = compiler
        .compile_into_spirv(
            fragment_shader.as_ref(),
            shaderc::ShaderKind::Fragment,
            &format!("{}.frag", name),
            "main",
            None,
        )
        .unwrap();
    let fragment_shader = device.create_shader_module(wgpu::ShaderModuleSource::SpirV(
        fragment_shader_spirv.as_binary(),
    ));

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        layout: &pipeline_layout,
        vertex_stage: wgpu::ProgrammableStageDescriptor {
            module: &vertex_shader,
            entry_point: "main",
        },
        fragment_stage: Some(wgpu::ProgrammableStageDescriptor {
            module: &fragment_shader,
            entry_point: "main",
        }),
        rasterization_state,
        primitive_topology,
        color_states,
        depth_stencil_state,
        vertex_state: wgpu::VertexStateDescriptor {
            index_format: wgpu::IndexFormat::Uint32,
            vertex_buffers: vertex_buffer_descriptors,
        },
        sample_count: 1,
        sample_mask: !0,
        alpha_to_coverage_enabled: false,
    });

    pipeline
}

/// Create a `wgpu::TextureDescriptor` appropriate for the provided texture data.
pub fn texture_descriptor<'a>(
    label: Option<&'a str>,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> wgpu::TextureDescriptor {
    wgpu::TextureDescriptor {
        label,
        size: wgpu::Extent3d {
            width,
            height,
            depth: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsage::COPY_DST | wgpu::TextureUsage::SAMPLED,
    }
}

pub fn create_texture<'a>(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: Option<&'a str>,
    width: u32,
    height: u32,
    data: &TextureData,
) -> wgpu::Texture {
    trace!(
        "Creating texture ({:?}: {}x{})",
        data.format(),
        width,
        height
    );
    let texture = device.create_texture(&texture_descriptor(label, width, height, data.format()));
    queue.write_texture(
        wgpu::TextureCopyView {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
        },
        data.data(),
        wgpu::TextureDataLayout {
            offset: 0,
            bytes_per_row: width * data.stride(),
            rows_per_image: 0,
        },
        wgpu::Extent3d {
            width,
            height,
            depth: 1,
        },
    );

    texture
}

pub struct DiffuseData<'a> {
    pub rgba: Cow<'a, [u8]>,
}

pub struct FullbrightData<'a> {
    pub fullbright: Cow<'a, [u8]>,
}

pub struct LightmapData<'a> {
    pub lightmap: Cow<'a, [u8]>,
}

pub enum TextureData<'a> {
    Diffuse(DiffuseData<'a>),
    Fullbright(FullbrightData<'a>),
    Lightmap(LightmapData<'a>),
}

impl<'a> TextureData<'a> {
    pub fn format(&self) -> wgpu::TextureFormat {
        match self {
            TextureData::Diffuse(_) => DIFFUSE_TEXTURE_FORMAT,
            TextureData::Fullbright(_) => FULLBRIGHT_TEXTURE_FORMAT,
            TextureData::Lightmap(_) => LIGHTMAP_TEXTURE_FORMAT,
        }
    }

    pub fn data(&self) -> &[u8] {
        match self {
            TextureData::Diffuse(d) => &d.rgba,
            TextureData::Fullbright(d) => &d.fullbright,
            TextureData::Lightmap(d) => &d.lightmap,
        }
    }

    pub fn stride(&self) -> u32 {
        (match self {
            TextureData::Diffuse(_) => size_of::<[u8; 4]>(),
            TextureData::Fullbright(_) => size_of::<u8>(),
            TextureData::Lightmap(_) => size_of::<u8>(),
        }) as u32
    }

    pub fn size(&self) -> wgpu::BufferAddress {
        self.data().len() as wgpu::BufferAddress
    }
}

pub struct Camera {
    origin: Vector3<f32>,
    angles: Vector3<Deg<f32>>,
    transform: Matrix4<f32>,
}

impl Camera {
    pub fn new(
        origin: Vector3<f32>,
        angles: Vector3<Deg<f32>>,
        projection: Matrix4<f32>,
    ) -> Camera {
        // convert coordinates
        let converted_origin = Vector3::new(-origin.y, origin.z, -origin.x);
        // translate the world by inverse of camera position
        let translation = Matrix4::from_translation(-converted_origin);
        let rotation = Matrix4::from(Euler::new(angles.x, -angles.y, -angles.z));

        Camera {
            origin,
            angles,
            transform: projection * rotation * translation,
        }
    }

    pub fn origin(&self) -> Vector3<f32> {
        self.origin
    }

    pub fn angles(&self) -> Vector3<Deg<f32>> {
        self.angles
    }

    pub fn transform(&self) -> Matrix4<f32> {
        self.transform
    }
}

#[derive(Clone, Copy, Debug)]
pub enum BindGroupLayoutId {
    PerFrame = 0,
    PerEntity = 1,
    PerTexture = 2,
    PerFace = 3,
}

// uniform float array elements are aligned as if they were vec4s
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug)]
pub struct UniformArrayFloat {
    value: f32,
}

#[repr(C, align(256))]
#[derive(Copy, Clone)]
// TODO: derive Debug once const generics are stable
pub struct FrameUniforms {
    // TODO: pack frame values into a [Vector4<f32>; 16],
    lightmap_anim_frames: [UniformArrayFloat; 64],
    camera_pos: Vector4<f32>,
    time: f32,
}

#[repr(C, align(256))]
#[derive(Clone, Copy, Debug)]
pub struct EntityUniforms {
    transform: Matrix4<f32>,
}

pub struct GraphicsState<'a> {
    device: wgpu::Device,
    queue: wgpu::Queue,
    depth_attachment: RefCell<wgpu::Texture>,

    bind_group_layouts: Vec<wgpu::BindGroupLayout>,
    bind_groups: Vec<wgpu::BindGroup>,

    frame_uniform_buffer: wgpu::Buffer,

    entity_uniform_buffer: RefCell<DynamicUniformBuffer<'a, EntityUniforms>>,
    diffuse_sampler: wgpu::Sampler,
    lightmap_sampler: wgpu::Sampler,

    alias_pipeline: wgpu::RenderPipeline,
    alias_bind_group_layouts: Vec<wgpu::BindGroupLayout>,

    brush_pipeline: wgpu::RenderPipeline,
    brush_bind_group_layouts: Vec<wgpu::BindGroupLayout>,
    brush_texture_uniform_buffer: RefCell<DynamicUniformBuffer<'a, brush::TextureUniforms>>,
    brush_texture_uniform_blocks: Vec<DynamicUniformBufferBlock<'a, brush::TextureUniforms>>,

    glyph_pipeline: wgpu::RenderPipeline,
    glyph_bind_group_layouts: Vec<wgpu::BindGroupLayout>,
    glyph_uniform_buffer: RefCell<DynamicUniformBuffer<'a, glyph::GlyphUniforms>>,

    quad_vertex_buffer: wgpu::Buffer,

    sprite_pipeline: wgpu::RenderPipeline,
    sprite_bind_group_layouts: Vec<wgpu::BindGroupLayout>,
    sprite_vertex_buffer: wgpu::Buffer,

    default_diffuse: wgpu::Texture,
    default_diffuse_view: wgpu::TextureView,
    default_fullbright: wgpu::Texture,
    default_fullbright_view: wgpu::TextureView,
    default_lightmap: wgpu::Texture,
    default_lightmap_view: wgpu::TextureView,

    palette: Palette,
    gfx_wad: Wad,
}

impl<'a> GraphicsState<'a> {
    pub fn new<'b>(
        device: wgpu::Device,
        queue: wgpu::Queue,
        width: u32,
        height: u32,
        vfs: &'b Vfs,
    ) -> Result<GraphicsState<'a>, Error> {
        let palette = Palette::load(&vfs, "gfx/palette.lmp");
        let gfx_wad = Wad::load(vfs.open("gfx.wad")?).unwrap();
        let mut compiler = shaderc::Compiler::new().unwrap();

        let depth_attachment = RefCell::new(device.create_texture(&wgpu::TextureDescriptor {
            label: Some("depth attachment"),
            size: wgpu::Extent3d {
                width,
                height,
                depth: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_ATTACHMENT_FORMAT,
            usage: wgpu::TextureUsage::OUTPUT_ATTACHMENT,
        }));

        let frame_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frame uniform buffer"),
            size: size_of::<FrameUniforms>() as wgpu::BufferAddress,
            usage: wgpu::BufferUsage::UNIFORM | wgpu::BufferUsage::COPY_DST,
            mapped_at_creation: false,
        });
        let entity_uniform_buffer = RefCell::new(DynamicUniformBuffer::new(&device));
        let brush_texture_uniform_buffer = RefCell::new(DynamicUniformBuffer::new(&device));
        let brush_texture_uniform_blocks = brush::TextureKind::iter()
            .map(|kind| {
                debug!("Texture kind: {:?} ({})", kind, kind as u32);
                brush_texture_uniform_buffer
                    .borrow_mut()
                    .allocate(brush::TextureUniforms { kind })
            })
            .collect();
        brush_texture_uniform_buffer.borrow_mut().flush(&queue);
        let glyph_uniform_buffer = RefCell::new(DynamicUniformBuffer::new(&device));

        let diffuse_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: None,
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            // TODO: these are the OpenGL defaults; see if there's a better choice for us
            lod_min_clamp: -1000.0,
            lod_max_clamp: 1000.0,
            compare: None,
            anisotropy_clamp: None,
            ..Default::default()
        });

        let lightmap_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: None,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            // TODO: these are the OpenGL defaults; see if there's a better choice for us
            lod_min_clamp: -1000.0,
            lod_max_clamp: 1000.0,
            compare: None,
            anisotropy_clamp: None,
            ..Default::default()
        });

        let bind_group_layouts: Vec<wgpu::BindGroupLayout> = BIND_GROUP_LAYOUT_DESCRIPTORS
            .iter()
            .map(|desc| device.create_bind_group_layout(desc))
            .collect();
        let bind_groups = vec![
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("per-frame bind group"),
                layout: &bind_group_layouts[BindGroupLayoutId::PerFrame as usize],
                bindings: &[wgpu::Binding {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(frame_uniform_buffer.slice(..)),
                }],
            }),
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("brush per-entity bind group"),
                layout: &bind_group_layouts[BindGroupLayoutId::PerEntity as usize],
                bindings: &[
                    wgpu::Binding {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(
                            entity_uniform_buffer
                                .borrow()
                                .buffer()
                                .slice(0..entity_uniform_buffer.borrow().block_size().get()),
                        ),
                    },
                    wgpu::Binding {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&diffuse_sampler),
                    },
                    wgpu::Binding {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&lightmap_sampler),
                    },
                ],
            }),
        ];

        let (alias_pipeline, alias_bind_group_layouts) =
            create_pipeline::<alias::AliasPipeline>(&device, &mut compiler, &bind_group_layouts);
        let (brush_pipeline, brush_bind_group_layouts) =
            create_pipeline::<brush::BrushPipeline>(&device, &mut compiler, &bind_group_layouts);
        let (sprite_pipeline, sprite_bind_group_layouts) =
            create_pipeline::<sprite::SpritePipeline>(&device, &mut compiler, &bind_group_layouts);
        let sprite_vertex_buffer = device.create_buffer_with_data(
            unsafe { any_slice_as_bytes(&sprite::VERTICES) },
            wgpu::BufferUsage::VERTEX,
        );

        // let (quad_pipeline, quad_bind_group_layouts) =
        //     create_pipeline::<quad::QuadPipeline>(&device, &mut compiler, &bind_group_layouts);
        let quad_vertex_buffer = device.create_buffer_with_data(
            unsafe { any_slice_as_bytes(&quad::VERTICES) },
            wgpu::BufferUsage::VERTEX,
        );
        let (glyph_pipeline, glyph_bind_group_layouts) =
            create_pipeline::<glyph::GlyphPipeline>(&device, &mut compiler, &[]);

        let default_diffuse = create_texture(
            &device,
            &queue,
            None,
            2,
            2,
            &TextureData::Diffuse(DiffuseData {
                // taking a page out of Valve's book with the pink-and-black checkerboard
                rgba: (&[
                    0xFF, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, 0xFF, 0xFF,
                    0x00, 0xFF, 0xFF,
                ][..])
                    .into(),
            }),
        );
        let default_fullbright = create_texture(
            &device,
            &queue,
            None,
            1,
            1,
            &TextureData::Fullbright(FullbrightData {
                fullbright: (&[0xFF][..]).into(),
            }),
        );
        let default_lightmap = create_texture(
            &device,
            &queue,
            None,
            1,
            1,
            &TextureData::Lightmap(LightmapData {
                lightmap: (&[0xFF][..]).into(),
            }),
        );

        let default_diffuse_view = default_diffuse.create_default_view();
        let default_fullbright_view = default_fullbright.create_default_view();
        let default_lightmap_view = default_lightmap.create_default_view();

        Ok(GraphicsState {
            device,
            queue,
            depth_attachment,
            frame_uniform_buffer,
            entity_uniform_buffer,

            bind_group_layouts,
            bind_groups,

            alias_pipeline,
            alias_bind_group_layouts,
            brush_pipeline,
            brush_bind_group_layouts,
            brush_texture_uniform_buffer,
            brush_texture_uniform_blocks,
            glyph_pipeline,
            glyph_bind_group_layouts,
            glyph_uniform_buffer,
            quad_vertex_buffer,
            sprite_pipeline,
            sprite_bind_group_layouts,
            sprite_vertex_buffer,
            diffuse_sampler,
            lightmap_sampler,
            default_diffuse,
            default_diffuse_view,
            default_fullbright,
            default_fullbright_view,
            default_lightmap,
            default_lightmap_view,
            palette,
            gfx_wad,
        })
    }

    pub fn create_texture<'b>(
        &self,
        label: Option<&'b str>,
        width: u32,
        height: u32,
        data: &TextureData,
    ) -> wgpu::Texture {
        create_texture(&self.device, &self.queue, label, width, height, data)
    }

    /// Creates a new depth attachment with the specified dimensions, replacing the old one.
    pub fn recreate_depth_attachment(&self, width: u32, height: u32) {
        let depth_attachment = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("depth attachment"),
            size: wgpu::Extent3d {
                width,
                height,
                depth: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_ATTACHMENT_FORMAT,
            usage: wgpu::TextureUsage::OUTPUT_ATTACHMENT,
        });
        let _ = self.depth_attachment.replace(depth_attachment);
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    pub fn depth_attachment(&self) -> Ref<wgpu::Texture> {
        self.depth_attachment.borrow()
    }

    pub fn frame_uniform_buffer(&self) -> &wgpu::Buffer {
        &self.frame_uniform_buffer
    }

    pub fn entity_uniform_buffer(&self) -> Ref<DynamicUniformBuffer<'a, EntityUniforms>> {
        self.entity_uniform_buffer.borrow()
    }

    pub fn entity_uniform_buffer_mut(&self) -> RefMut<DynamicUniformBuffer<'a, EntityUniforms>> {
        self.entity_uniform_buffer.borrow_mut()
    }

    pub fn diffuse_sampler(&self) -> &wgpu::Sampler {
        &self.diffuse_sampler
    }

    pub fn default_lightmap_view(&self) -> &wgpu::TextureView {
        &self.default_lightmap_view
    }

    pub fn lightmap_sampler(&self) -> &wgpu::Sampler {
        &self.lightmap_sampler
    }

    pub fn bind_group_layouts(&self) -> &[wgpu::BindGroupLayout] {
        &self.bind_group_layouts
    }

    pub fn alias_pipeline(&self) -> &wgpu::RenderPipeline {
        &self.alias_pipeline
    }

    pub fn alias_bind_group_layout(&self, id: BindGroupLayoutId) -> &wgpu::BindGroupLayout {
        &self.alias_bind_group_layouts[id as usize - 2]
    }

    // brush pipeline

    pub fn brush_pipeline(&self) -> &wgpu::RenderPipeline {
        &self.brush_pipeline
    }

    pub fn brush_bind_group_layout(&self, id: BindGroupLayoutId) -> &wgpu::BindGroupLayout {
        &self.brush_bind_group_layouts[id as usize - 2]
    }

    pub fn brush_bind_group_layouts(&self) -> &[wgpu::BindGroupLayout] {
        &self.brush_bind_group_layouts
    }

    pub fn brush_texture_uniform_buffer(
        &self,
    ) -> Ref<DynamicUniformBuffer<'a, brush::TextureUniforms>> {
        self.brush_texture_uniform_buffer.borrow()
    }

    pub fn brush_texture_uniform_buffer_mut(
        &self,
    ) -> RefMut<DynamicUniformBuffer<'a, brush::TextureUniforms>> {
        self.brush_texture_uniform_buffer.borrow_mut()
    }

    pub fn brush_texture_uniform_block(
        &self,
        kind: brush::TextureKind,
    ) -> &DynamicUniformBufferBlock<'a, brush::TextureUniforms> {
        &self.brush_texture_uniform_blocks[kind as usize]
    }

    // glyph pipeline

    pub fn glyph_pipeline(&self) -> &wgpu::RenderPipeline {
        &self.glyph_pipeline
    }

    pub fn glyph_bind_group_layouts(&self) -> &[wgpu::BindGroupLayout] {
        &self.glyph_bind_group_layouts
    }

    pub fn glyph_uniform_buffer(&self) -> Ref<DynamicUniformBuffer<'a, glyph::GlyphUniforms>> {
        self.glyph_uniform_buffer.borrow()
    }

    pub fn glyph_uniform_buffer_mut(
        &self,
    ) -> RefMut<DynamicUniformBuffer<'a, glyph::GlyphUniforms>> {
        self.glyph_uniform_buffer.borrow_mut()
    }

    // quad pipeline(s)

    pub fn quad_vertex_buffer(&self) -> &wgpu::Buffer {
        &self.quad_vertex_buffer
    }

    // sprite pipeline

    pub fn sprite_pipeline(&self) -> &wgpu::RenderPipeline {
        &self.sprite_pipeline
    }

    pub fn sprite_bind_group_layout(&self, id: BindGroupLayoutId) -> &wgpu::BindGroupLayout {
        &self.sprite_bind_group_layouts[id as usize - 2]
    }

    pub fn sprite_bind_group_layouts(&self) -> &[wgpu::BindGroupLayout] {
        &self.sprite_bind_group_layouts
    }

    pub fn sprite_vertex_buffer(&self) -> &wgpu::Buffer {
        &self.sprite_vertex_buffer
    }

    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    pub fn gfx_wad(&self) -> &Wad {
        &self.gfx_wad
    }
}

enum EntityRenderer<'a> {
    Alias(AliasRenderer),
    Brush(BrushRenderer<'a>),
    Sprite(SpriteRenderer),
    None,
}

/// Top-level renderer.
pub struct Renderer<'a> {
    state: Rc<GraphicsState<'a>>,

    world_renderer: BrushRenderer<'a>,
    entity_renderers: Vec<EntityRenderer<'a>>,
    glyph_renderer: GlyphRenderer,

    world_uniform_block: DynamicUniformBufferBlock<'a, EntityUniforms>,
    entity_uniform_blocks: RefCell<Vec<DynamicUniformBufferBlock<'a, EntityUniforms>>>,
    glyph_uniform_blocks: RefCell<Vec<DynamicUniformBufferBlock<'a, GlyphUniforms>>>,
}

impl<'a> Renderer<'a> {
    pub fn new(
        models: &[Model],
        worldmodel_id: usize,
        state: Rc<GraphicsState<'a>>,
    ) -> Renderer<'a> {
        let mut world_renderer = None;
        let mut entity_renderers = Vec::new();

        let world_uniform_block = state.entity_uniform_buffer_mut().allocate(EntityUniforms {
            transform: Matrix4::identity(),
        });

        for (i, model) in models.iter().enumerate() {
            if i == worldmodel_id {
                match *model.kind() {
                    ModelKind::Brush(ref bmodel) => {
                        world_renderer = Some(
                            BrushRendererBuilder::new(bmodel, state.clone(), true)
                                .build()
                                .unwrap(),
                        );
                    }
                    _ => panic!("Invalid worldmodel"),
                }
            } else {
                match *model.kind() {
                    ModelKind::Alias(ref amodel) => entity_renderers.push(EntityRenderer::Alias(
                        AliasRenderer::new(state.clone(), amodel).unwrap(),
                    )),

                    ModelKind::Brush(ref bmodel) => {
                        entity_renderers.push(EntityRenderer::Brush(
                            BrushRendererBuilder::new(bmodel, state.clone(), false)
                                .build()
                                .unwrap(),
                        ));
                    }

                    ModelKind::Sprite(ref smodel) => {
                        entity_renderers
                            .push(EntityRenderer::Sprite(SpriteRenderer::new(&state, smodel)));
                    }

                    _ => {
                        warn!("Non-brush renderers not implemented!");
                        entity_renderers.push(EntityRenderer::None);
                    }
                }
            }
        }

        Renderer {
            state: state.clone(),
            world_renderer: world_renderer.unwrap(),
            entity_renderers,
            glyph_renderer: GlyphRenderer::new(&state),
            world_uniform_block,
            entity_uniform_blocks: RefCell::new(Vec::new()),
            glyph_uniform_blocks: RefCell::new(Vec::new()),
        }
    }

    pub fn update_uniform_buffers<'b, I>(
        &'b self,
        camera: &Camera,
        display_width: u32,
        display_height: u32,
        time: Duration,
        entities: I,
        lightstyle_values: &[f32],
    ) where
        I: Iterator<Item = &'b ClientEntity>,
    {
        let _guard = flame::start_guard("Renderer::update_uniform");

        let device = self.state.device();

        println!("time = {:?}", engine::duration_to_f32(time));
        trace!("Updating frame uniform buffer");
        self.state
            .queue()
            .write_buffer(self.state.frame_uniform_buffer(), 0, unsafe {
                any_as_bytes(&FrameUniforms {
                    lightmap_anim_frames: {
                        let mut frames = [UniformArrayFloat { value: 0.0 }; 64];
                        for i in 0..64 {
                            frames[i].value = lightstyle_values[i];
                        }
                        frames
                    },
                    camera_pos: camera.origin.extend(1.0),
                    time: engine::duration_to_f32(time),
                })
            });

        trace!("Updating entity uniform buffer");
        let queue = self.state.queue();
        let world_uniforms = EntityUniforms {
            transform: camera.transform(),
        };
        self.state
            .entity_uniform_buffer_mut()
            .write_block(&self.world_uniform_block, world_uniforms);

        for (ent_pos, ent) in entities.into_iter().enumerate() {
            let ent_uniforms = EntityUniforms {
                transform: self.calculate_transform(camera, ent),
            };

            if ent_pos >= self.entity_uniform_blocks.borrow().len() {
                // if we don't have enough blocks, get a new one
                let block = self
                    .state
                    .entity_uniform_buffer_mut()
                    .allocate(ent_uniforms);
                self.entity_uniform_blocks.borrow_mut().push(block);
            } else {
                self.state
                    .entity_uniform_buffer_mut()
                    .write_block(&self.entity_uniform_blocks.borrow()[ent_pos], ent_uniforms);
            }
        }

        self.state.entity_uniform_buffer().flush(self.state.queue());

        trace!("Updating glyph uniform buffer");
        // TODO: generate actual commands
        let glyph_commands = vec![GlyphRendererCommand::Text {
            text: "The Quick Brown Fox Jumps Over The Lazy Dog".to_string(),
            x: 0,
            y: 0,
        }];

        let glyph_uniforms =
            self.glyph_renderer
                .generate_uniforms(&glyph_commands, display_width, display_height);

        self.glyph_uniform_blocks.borrow_mut().clear();
        self.state.glyph_uniform_buffer_mut().clear().unwrap();
        for (uni_id, uni) in glyph_uniforms.into_iter().enumerate() {
            if uni_id >= self.glyph_uniform_blocks.borrow().len() {
                let block = self.state.glyph_uniform_buffer_mut().allocate(uni);
                self.glyph_uniform_blocks.borrow_mut().push(block);
            } else {
                self.state
                    .glyph_uniform_buffer_mut()
                    .write_block(&self.glyph_uniform_blocks.borrow()[uni_id], uni);
            }
        }
        self.state.glyph_uniform_buffer_mut().flush(self.state.queue());
    }

    pub fn render_pass<'b, I>(
        &'b self,
        color_attachment_view: &wgpu::TextureView,
        camera: &Camera,
        display_width: u32,
        display_height: u32,
        time: Duration,
        entities: I,
        lightstyle_values: &[f32],
    ) where
        I: Iterator<Item = &'b ClientEntity> + Clone,
    {
        let _guard = flame::start_guard("Renderer::render_pass");
        let mut encoder = self
            .state
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        let depth_view = self.state.depth_attachment().create_default_view();
        {
            info!("Updating uniform buffers");
            self.update_uniform_buffers(
                camera,
                display_width,
                display_height,
                time,
                entities.clone(),
                lightstyle_values,
            );

            info!("Beginning render pass");
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[wgpu::RenderPassColorAttachmentDescriptor {
                    attachment: color_attachment_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 1.0,
                        }),
                        store: true,
                    },
                }],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachmentDescriptor {
                    attachment: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: true,
                    }),
                    stencil_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: true,
                    }),
                }),
            });

            pass.set_bind_group(
                BindGroupLayoutId::PerFrame as u32,
                &self.state.bind_groups[BindGroupLayoutId::PerFrame as usize],
                &[],
            );

            // draw world
            info!("Drawing world");
            pass.set_bind_group(
                BindGroupLayoutId::PerEntity as u32,
                &self.state.bind_groups[BindGroupLayoutId::PerEntity as usize],
                &[self.world_uniform_block.offset()],
            );
            self.world_renderer
                .record_draw(&mut pass, &self.world_uniform_block, camera);

            // draw entities
            info!("Drawing entities");
            for (ent_pos, ent) in entities.enumerate() {
                let model_id = ent.get_model_id();

                pass.set_bind_group(
                    BindGroupLayoutId::PerEntity as u32,
                    &self.state.bind_groups[BindGroupLayoutId::PerEntity as usize],
                    &[self.entity_uniform_blocks.borrow()[ent_pos].offset()],
                );

                match self.renderer_for_entity(&ent) {
                    EntityRenderer::Brush(ref bmodel) => bmodel.record_draw(
                        &mut pass,
                        &self.entity_uniform_blocks.borrow()[ent_pos],
                        camera,
                    ),
                    EntityRenderer::Alias(ref alias) => alias.record_draw(
                        &self.state,
                        &mut pass,
                        time,
                        ent.get_frame_id(),
                        ent.get_skin_id(),
                    ),
                    EntityRenderer::Sprite(ref sprite) => {
                        sprite.record_draw(&self.state, &mut pass, ent.get_frame_id(), time)
                    }
                    _ => warn!("non-brush renderers not implemented!"),
                    // _ => unimplemented!(),
                }
            }

            // draw text
            info!("Drawing text");
            self.glyph_renderer.record_draw(
                &self.state,
                &mut pass,
                &self.glyph_uniform_blocks.borrow(),
            );
        }

        let command_buffer = encoder.finish();
        {
            let _submit_guard = flame::start_guard("Submit and poll");
            self.state.queue().submit(vec![command_buffer]);
            self.state.device().poll(wgpu::Maintain::Wait);
        }
    }

    fn renderer_for_entity(&self, ent: &ClientEntity) -> &EntityRenderer<'a> {
        // subtract 1 from index because world entity isn't counted
        &self.entity_renderers[ent.get_model_id() - 1]
    }

    fn calculate_transform(&self, camera: &Camera, entity: &ClientEntity) -> Matrix4<f32> {
        let origin = entity.get_origin();
        let angles = entity.get_angles();
        let euler = match self.renderer_for_entity(entity) {
            EntityRenderer::Sprite(ref sprite) => match sprite.kind() {
                // used for decals
                SpriteKind::Oriented => Euler::new(angles.x, angles.y, angles.z),

                _ => {
                    // keep sprite facing player, but preserve roll
                    let inv_cam_angles = -camera.angles();
                    Euler::new(inv_cam_angles.x, inv_cam_angles.y, angles.z)
                }
            },

            _ => Euler::new(angles.x, angles.y, angles.z),
        };

        camera.transform()
            * Matrix4::from_translation(Vector3::new(-origin.y, origin.z, -origin.x))
            * Matrix4::from(euler)
    }
}
