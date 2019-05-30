//! Methods and types to control dynamic textures
//!
//! A dynamic texture is a texture from which you can spawn sprites. The `dynamic` part of the name
//! refers to the sprites. A sprite is a rectangular view into the texture. The sprites can be
//! changed freely during runtime. This allows movement of sprites, animations, and warping of
//! their form.
use super::utils::*;
use crate::data::{DrawType, SingleTexture, VxDraw};
use ::image as load_image;
use cgmath::Matrix4;
use cgmath::Rad;
use core::ptr::read;
#[cfg(feature = "dx12")]
use gfx_backend_dx12 as back;
#[cfg(feature = "gl")]
use gfx_backend_gl as back;
#[cfg(feature = "metal")]
use gfx_backend_metal as back;
#[cfg(feature = "vulkan")]
use gfx_backend_vulkan as back;
use gfx_hal::{
    adapter::PhysicalDevice,
    command,
    device::Device,
    format, image, memory,
    memory::Properties,
    pass,
    pso::{self, DescriptorPool},
    Backend, Primitive,
};
use std::mem::{size_of, ManuallyDrop};

// ---

/// A view into a texture
pub struct Handle(usize, usize);

/// Handle to a texture
pub struct Layer(usize);

impl Layerable for Layer {
    fn get_layer(&self, vx: &VxDraw) -> usize {
        for (idx, ord) in vx.draw_order.iter().enumerate() {
            match ord {
                DrawType::DynamicTexture { id } if *id == self.0 => {
                    return idx;
                }
                _ => {}
            }
        }
        panic!["Unable to get layer"]
    }
}

/// Options for creating a layer of a dynamic texture with sprites
#[derive(Clone, Copy)]
pub struct LayerOptions {
    /// Perform depth testing (and fragment culling) when drawing sprites from this texture
    depth_test: bool,
    /// Fix the perspective, this ignores the perspective sent into draw for this texture and
    /// all its associated sprites
    fixed_perspective: Option<Matrix4<f32>>,
}

impl LayerOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn depth(mut self, depth: bool) -> Self {
        self.depth_test = depth;
        self
    }

    pub fn fixed_perspective(mut self, mat: Matrix4<f32>) -> Self {
        self.fixed_perspective = Some(mat);
        self
    }
}

impl Default for LayerOptions {
    fn default() -> Self {
        Self {
            depth_test: true,
            fixed_perspective: None,
        }
    }
}

/// Sprite creation builder
///
/// A sprite is a rectangular view into a texture. This structure sets up the necessary data to
/// call [Dyntex::add] with.
#[derive(Clone, Copy)]
pub struct Sprite {
    width: f32,
    height: f32,
    depth: f32,
    colors: [(u8, u8, u8, u8); 4],
    uv_begin: (f32, f32),
    uv_end: (f32, f32),
    translation: (f32, f32),
    rotation: f32,
    scale: f32,
    origin: (f32, f32),
}

impl Sprite {
    /// Same as default
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the width of the sprite
    pub fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    /// Set the height of the sprite
    pub fn height(mut self, height: f32) -> Self {
        self.height = height;
        self
    }

    /// Set the colors of the sprite
    ///
    /// The colors are added on top of whatever the sprite's texture data is
    pub fn colors(mut self, colors: [(u8, u8, u8, u8); 4]) -> Self {
        self.colors = colors;
        self
    }

    /// Set the topleft corner's UV coordinates
    pub fn uv_begin(mut self, uv: (f32, f32)) -> Self {
        self.uv_begin = uv;
        self
    }

    /// Set the bottom right corner's UV coordinates
    pub fn uv_end(mut self, uv: (f32, f32)) -> Self {
        self.uv_end = uv;
        self
    }

    /// Set the translation
    pub fn translation(mut self, trn: (f32, f32)) -> Self {
        self.translation = trn;
        self
    }

    /// Set the rotation. Rotation is counter-clockwise
    pub fn rotation(mut self, rot: f32) -> Self {
        self.rotation = rot;
        self
    }

    /// Set the scaling factor of this sprite
    pub fn scale(mut self, scale: f32) -> Self {
        self.scale = scale;
        self
    }

    /// Set the origin of this sprite
    pub fn origin(mut self, origin: (f32, f32)) -> Self {
        self.origin = origin;
        self
    }
}

impl Default for Sprite {
    fn default() -> Self {
        Sprite {
            width: 2.0,
            height: 2.0,
            depth: 0.0,
            colors: [(0, 0, 0, 255); 4],
            uv_begin: (0.0, 0.0),
            uv_end: (1.0, 1.0),
            translation: (0.0, 0.0),
            rotation: 0.0,
            scale: 1.0,
            origin: (0.0, 0.0),
        }
    }
}

// ---

/// Accessor object to all dynamic textures
///
/// A dynamic texture is a texture which is used to display textured sprites.
/// See [crate::dyntex] for examples.
pub struct Dyntex<'a> {
    vx: &'a mut VxDraw,
}

impl<'a> Dyntex<'a> {
    /// Prepare to edit dynamic textures
    ///
    /// You're not supposed to use this function directly (although you can).
    /// The recommended way of spawning a dyntex is via [VxDraw::dyntex()].
    pub fn new(s: &'a mut VxDraw) -> Self {
        Self { vx: s }
    }

    /// Disable drawing of the sprites at this layer
    pub fn hide(&mut self, layer: &Layer) {
        self.vx.dyntexs[layer.0].hidden = true;
    }

    /// Enable drawing of the sprites at this layer
    pub fn show(&mut self, layer: &Layer) {
        self.vx.dyntexs[layer.0].hidden = false;
    }

    /// Add a texture (layer) to the system
    ///
    /// You use a texture to create sprites. Sprites are rectangular views into a texture. Sprites
    /// based on different texures are drawn in the order in which the textures were allocated, that
    /// means that the first texture's sprites are drawn first, then, the second texture's sprites,and
    /// so on.
    ///
    /// Each texture has options (See [dyntex::LayerOptions]). This decides how the derivative sprites are
    /// drawn.
    ///
    /// Note: Alpha blending with depth testing will make foreground transparency not be transparent.
    /// To make sure transparency works correctly you can turn off the depth test for foreground
    /// objects and ensure that the foreground texture is allocated last.
    pub fn add_layer(&mut self, img_data: &[u8], options: LayerOptions) -> Layer {
        let s = &mut *self.vx;
        let device = &s.device;

        let img = load_image::load_from_memory_with_format(&img_data[..], load_image::PNG)
            .unwrap()
            .to_rgba();

        let pixel_size = 4; //size_of::<image::Rgba<u8>>();
        let row_size = pixel_size * (img.width() as usize);
        let limits = s.adapter.physical_device.limits();
        let row_alignment_mask = limits.optimal_buffer_copy_pitch_alignment as u32 - 1;
        let row_pitch = ((row_size as u32 + row_alignment_mask) & !row_alignment_mask) as usize;
        debug_assert!(row_pitch as usize >= row_size);
        let required_bytes = row_pitch * img.height() as usize;

        let mut image_upload_buffer = unsafe {
            device.create_buffer(required_bytes as u64, gfx_hal::buffer::Usage::TRANSFER_SRC)
        }
        .unwrap();
        let image_mem_reqs = unsafe { device.get_buffer_requirements(&image_upload_buffer) };
        let memory_type_id =
            find_memory_type_id(&s.adapter, image_mem_reqs, Properties::CPU_VISIBLE);
        let image_upload_memory =
            unsafe { device.allocate_memory(memory_type_id, image_mem_reqs.size) }.unwrap();
        unsafe { device.bind_buffer_memory(&image_upload_memory, 0, &mut image_upload_buffer) }
            .unwrap();

        unsafe {
            let mut writer = s
                .device
                .acquire_mapping_writer::<u8>(&image_upload_memory, 0..image_mem_reqs.size)
                .expect("Unable to get mapping writer");
            for y in 0..img.height() as usize {
                let row = &(*img)[y * row_size..(y + 1) * row_size];
                let dest_base = y * row_pitch;
                writer[dest_base..dest_base + row.len()].copy_from_slice(row);
            }
            device
                .release_mapping_writer(writer)
                .expect("Couldn't release the mapping writer to the staging buffer!");
        }

        let mut the_image = unsafe {
            device
                .create_image(
                    image::Kind::D2(img.width(), img.height(), 1, 1),
                    1,
                    format::Format::Rgba8Srgb,
                    image::Tiling::Optimal,
                    image::Usage::TRANSFER_DST | image::Usage::SAMPLED,
                    image::ViewCapabilities::empty(),
                )
                .expect("Couldn't create the image!")
        };

        let image_memory = unsafe {
            let requirements = device.get_image_requirements(&the_image);
            let memory_type_id =
                find_memory_type_id(&s.adapter, requirements, memory::Properties::DEVICE_LOCAL);
            device
                .allocate_memory(memory_type_id, requirements.size)
                .expect("Unable to allocate")
        };

        let image_view = unsafe {
            device
                .bind_image_memory(&image_memory, 0, &mut the_image)
                .expect("Unable to bind memory");

            device
                .create_image_view(
                    &the_image,
                    image::ViewKind::D2,
                    format::Format::Rgba8Srgb,
                    format::Swizzle::NO,
                    image::SubresourceRange {
                        aspects: format::Aspects::COLOR,
                        levels: 0..1,
                        layers: 0..1,
                    },
                )
                .expect("Couldn't create the image view!")
        };

        let sampler = unsafe {
            s.device
                .create_sampler(image::SamplerInfo::new(
                    image::Filter::Nearest,
                    image::WrapMode::Tile,
                ))
                .expect("Couldn't create the sampler!")
        };

        unsafe {
            let mut cmd_buffer = s.command_pool.acquire_command_buffer::<command::OneShot>();
            cmd_buffer.begin();
            let image_barrier = memory::Barrier::Image {
                states: (image::Access::empty(), image::Layout::Undefined)
                    ..(
                        image::Access::TRANSFER_WRITE,
                        image::Layout::TransferDstOptimal,
                    ),
                target: &the_image,
                families: None,
                range: image::SubresourceRange {
                    aspects: format::Aspects::COLOR,
                    levels: 0..1,
                    layers: 0..1,
                },
            };
            cmd_buffer.pipeline_barrier(
                pso::PipelineStage::TOP_OF_PIPE..pso::PipelineStage::TRANSFER,
                memory::Dependencies::empty(),
                &[image_barrier],
            );
            cmd_buffer.copy_buffer_to_image(
                &image_upload_buffer,
                &the_image,
                image::Layout::TransferDstOptimal,
                &[command::BufferImageCopy {
                    buffer_offset: 0,
                    buffer_width: (row_pitch / pixel_size) as u32,
                    buffer_height: img.height(),
                    image_layers: gfx_hal::image::SubresourceLayers {
                        aspects: format::Aspects::COLOR,
                        level: 0,
                        layers: 0..1,
                    },
                    image_offset: image::Offset { x: 0, y: 0, z: 0 },
                    image_extent: image::Extent {
                        width: img.width(),
                        height: img.height(),
                        depth: 1,
                    },
                }],
            );
            let image_barrier = memory::Barrier::Image {
                states: (
                    image::Access::TRANSFER_WRITE,
                    image::Layout::TransferDstOptimal,
                )
                    ..(
                        image::Access::SHADER_READ,
                        image::Layout::ShaderReadOnlyOptimal,
                    ),
                target: &the_image,
                families: None,
                range: image::SubresourceRange {
                    aspects: format::Aspects::COLOR,
                    levels: 0..1,
                    layers: 0..1,
                },
            };
            cmd_buffer.pipeline_barrier(
                pso::PipelineStage::TRANSFER..pso::PipelineStage::FRAGMENT_SHADER,
                memory::Dependencies::empty(),
                &[image_barrier],
            );
            cmd_buffer.finish();
            let upload_fence = s
                .device
                .create_fence(false)
                .expect("Couldn't create an upload fence!");
            s.queue_group.queues[0].submit_nosemaphores(Some(&cmd_buffer), Some(&upload_fence));
            s.device
                .wait_for_fence(&upload_fence, u64::max_value())
                .expect("Couldn't wait for the fence!");
            s.device.destroy_fence(upload_fence);
        }

        unsafe {
            device.destroy_buffer(image_upload_buffer);
            device.free_memory(image_upload_memory);
        }

        const VERTEX_SOURCE_TEXTURE: &[u8] = include_bytes!["../_build/spirv/dyntex.vert.spirv"];

        const FRAGMENT_SOURCE_TEXTURE: &[u8] = include_bytes!["../_build/spirv/dyntex.frag.spirv"];

        let vs_module =
            { unsafe { s.device.create_shader_module(&VERTEX_SOURCE_TEXTURE) }.unwrap() };
        let fs_module =
            { unsafe { s.device.create_shader_module(&FRAGMENT_SOURCE_TEXTURE) }.unwrap() };

        // Describe the shaders
        const ENTRY_NAME: &str = "main";
        let vs_module: <back::Backend as Backend>::ShaderModule = vs_module;
        let (vs_entry, fs_entry) = (
            pso::EntryPoint {
                entry: ENTRY_NAME,
                module: &vs_module,
                specialization: pso::Specialization::default(),
            },
            pso::EntryPoint {
                entry: ENTRY_NAME,
                module: &fs_module,
                specialization: pso::Specialization::default(),
            },
        );
        let shader_entries = pso::GraphicsShaderSet {
            vertex: vs_entry,
            hull: None,
            domain: None,
            geometry: None,
            fragment: Some(fs_entry),
        };
        let input_assembler = pso::InputAssemblerDesc::new(Primitive::TriangleList);

        let vertex_buffers: Vec<pso::VertexBufferDesc> = vec![pso::VertexBufferDesc {
            binding: 0,
            stride: (size_of::<f32>() * (3 + 2 + 2 + 2 + 1)) as u32,
            rate: pso::VertexInputRate::Vertex,
        }];
        let attributes: Vec<pso::AttributeDesc> = vec![
            pso::AttributeDesc {
                location: 0,
                binding: 0,
                element: pso::Element {
                    format: format::Format::Rgb32Sfloat,
                    offset: 0,
                },
            },
            pso::AttributeDesc {
                location: 1,
                binding: 0,
                element: pso::Element {
                    format: format::Format::Rg32Sfloat,
                    offset: 12,
                },
            },
            pso::AttributeDesc {
                location: 2,
                binding: 0,
                element: pso::Element {
                    format: format::Format::Rg32Sfloat,
                    offset: 20,
                },
            },
            pso::AttributeDesc {
                location: 3,
                binding: 0,
                element: pso::Element {
                    format: format::Format::R32Sfloat,
                    offset: 28,
                },
            },
            pso::AttributeDesc {
                location: 4,
                binding: 0,
                element: pso::Element {
                    format: format::Format::R32Sfloat,
                    offset: 32,
                },
            },
            pso::AttributeDesc {
                location: 5,
                binding: 0,
                element: pso::Element {
                    format: format::Format::Rgba8Unorm,
                    offset: 36,
                },
            },
        ];

        let rasterizer = pso::Rasterizer {
            depth_clamping: false,
            polygon_mode: pso::PolygonMode::Fill,
            cull_face: pso::Face::NONE,
            front_face: pso::FrontFace::Clockwise,
            depth_bias: None,
            conservative: false,
        };

        let depth_stencil = pso::DepthStencilDesc {
            depth: if options.depth_test {
                pso::DepthTest::On {
                    fun: pso::Comparison::LessEqual,
                    write: true,
                }
            } else {
                pso::DepthTest::Off
            },
            depth_bounds: false,
            stencil: pso::StencilTest::Off,
        };
        let blender = {
            let blend_state = pso::BlendState::On {
                color: pso::BlendOp::Add {
                    src: pso::Factor::SrcAlpha,
                    dst: pso::Factor::OneMinusSrcAlpha,
                },
                alpha: pso::BlendOp::Add {
                    src: pso::Factor::One,
                    dst: pso::Factor::OneMinusSrcAlpha,
                },
            };
            pso::BlendDesc {
                logic_op: Some(pso::LogicOp::Copy),
                targets: vec![pso::ColorBlendDesc(pso::ColorMask::ALL, blend_state)],
            }
        };

        let triangle_render_pass = {
            let attachment = pass::Attachment {
                format: Some(s.format),
                samples: 1,
                ops: pass::AttachmentOps::new(
                    pass::AttachmentLoadOp::Clear,
                    pass::AttachmentStoreOp::Store,
                ),
                stencil_ops: pass::AttachmentOps::DONT_CARE,
                layouts: image::Layout::Undefined..image::Layout::Present,
            };
            let depth = pass::Attachment {
                format: Some(format::Format::D32Sfloat),
                samples: 1,
                ops: pass::AttachmentOps::new(
                    pass::AttachmentLoadOp::Clear,
                    pass::AttachmentStoreOp::Store,
                ),
                stencil_ops: pass::AttachmentOps::DONT_CARE,
                layouts: image::Layout::Undefined..image::Layout::DepthStencilAttachmentOptimal,
            };

            let subpass = pass::SubpassDesc {
                colors: &[(0, image::Layout::ColorAttachmentOptimal)],
                depth_stencil: Some(&(1, image::Layout::DepthStencilAttachmentOptimal)),
                inputs: &[],
                resolves: &[],
                preserves: &[],
            };

            unsafe {
                s.device
                    .create_render_pass(&[attachment, depth], &[subpass], &[])
            }
            .expect("Can't create render pass")
        };
        let baked_states = pso::BakedStates {
            viewport: None,
            scissor: None,
            blend_color: None,
            depth_bounds: None,
        };
        let mut bindings = Vec::<pso::DescriptorSetLayoutBinding>::new();
        bindings.push(pso::DescriptorSetLayoutBinding {
            binding: 0,
            ty: pso::DescriptorType::SampledImage,
            count: 1,
            stage_flags: pso::ShaderStageFlags::FRAGMENT,
            immutable_samplers: false,
        });
        bindings.push(pso::DescriptorSetLayoutBinding {
            binding: 1,
            ty: pso::DescriptorType::Sampler,
            count: 1,
            stage_flags: pso::ShaderStageFlags::FRAGMENT,
            immutable_samplers: false,
        });
        let immutable_samplers = Vec::<<back::Backend as Backend>::Sampler>::new();
        let triangle_descriptor_set_layouts: Vec<<back::Backend as Backend>::DescriptorSetLayout> =
            vec![unsafe {
                s.device
                    .create_descriptor_set_layout(bindings, immutable_samplers)
                    .expect("Couldn't make a DescriptorSetLayout")
            }];

        let mut descriptor_pool = unsafe {
            s.device
                .create_descriptor_pool(
                    1, // sets
                    &[
                        pso::DescriptorRangeDesc {
                            ty: pso::DescriptorType::SampledImage,
                            count: 1,
                        },
                        pso::DescriptorRangeDesc {
                            ty: pso::DescriptorType::Sampler,
                            count: 1,
                        },
                    ],
                    pso::DescriptorPoolCreateFlags::empty(),
                )
                .expect("Couldn't create a descriptor pool!")
        };

        let descriptor_set = unsafe {
            descriptor_pool
                .allocate_set(&triangle_descriptor_set_layouts[0])
                .expect("Couldn't make a Descriptor Set!")
        };

        unsafe {
            s.device.write_descriptor_sets(vec![
                pso::DescriptorSetWrite {
                    set: &descriptor_set,
                    binding: 0,
                    array_offset: 0,
                    descriptors: Some(pso::Descriptor::Image(
                        &image_view,
                        image::Layout::ShaderReadOnlyOptimal,
                    )),
                },
                pso::DescriptorSetWrite {
                    set: &descriptor_set,
                    binding: 1,
                    array_offset: 0,
                    descriptors: Some(pso::Descriptor::Sampler(&sampler)),
                },
            ]);
        }

        let mut push_constants = Vec::<(pso::ShaderStageFlags, core::ops::Range<u32>)>::new();
        push_constants.push((pso::ShaderStageFlags::VERTEX, 0..16));
        let triangle_pipeline_layout = unsafe {
            s.device
                .create_pipeline_layout(&triangle_descriptor_set_layouts, push_constants)
                .expect("Couldn't create a pipeline layout")
        };

        // Describe the pipeline (rasterization, triangle interpretation)
        let pipeline_desc = pso::GraphicsPipelineDesc {
            shaders: shader_entries,
            rasterizer,
            vertex_buffers,
            attributes,
            input_assembler,
            blender,
            depth_stencil,
            multisampling: None,
            baked_states,
            layout: &triangle_pipeline_layout,
            subpass: pass::Subpass {
                index: 0,
                main_pass: &triangle_render_pass,
            },
            flags: pso::PipelineCreationFlags::empty(),
            parent: pso::BasePipeline::None,
        };

        let triangle_pipeline = unsafe {
            s.device
                .create_graphics_pipeline(&pipeline_desc, None)
                .expect("Couldn't create a graphics pipeline!")
        };

        unsafe {
            s.device.destroy_shader_module(vs_module);
            s.device.destroy_shader_module(fs_module);
        }

        let texture_vertex_sprites = super::utils::ResizBuf::new(&s.device, &s.adapter);
        let indices = super::utils::ResizBufIdx4::new(&s.device, &s.adapter);

        s.dyntexs.push(SingleTexture {
            hidden: false,
            count: 0,

            fixed_perspective: options.fixed_perspective,
            mockbuffer: vec![],
            removed: vec![],

            texture_vertex_sprites,
            indices,

            texture_image_buffer: ManuallyDrop::new(the_image),
            texture_image_memory: ManuallyDrop::new(image_memory),

            descriptor_pool: ManuallyDrop::new(descriptor_pool),
            image_view: ManuallyDrop::new(image_view),
            sampler: ManuallyDrop::new(sampler),

            descriptor_set: ManuallyDrop::new(descriptor_set),
            descriptor_set_layouts: triangle_descriptor_set_layouts,
            pipeline: ManuallyDrop::new(triangle_pipeline),
            pipeline_layout: ManuallyDrop::new(triangle_pipeline_layout),
            render_pass: ManuallyDrop::new(triangle_render_pass),
        });
        s.draw_order.push(DrawType::DynamicTexture {
            id: s.dyntexs.len() - 1,
        });
        Layer(s.dyntexs.len() - 1)
    }

    /// Add a sprite (a rectangular view of a texture) to the system
    ///
    /// The sprite is automatically drawn on each [crate::VxDraw::draw_frame] call, and must be removed by
    /// [crate::dyntex::Dyntex::remove_sprite] to stop it from being drawn.
    pub fn add(&mut self, texture: &Layer, sprite: Sprite) -> Handle {
        let s = &mut *self.vx;
        let tex = &mut s.dyntexs[texture.0];

        // Derive xy from the sprite's initial UV
        let uv_a = sprite.uv_begin;
        let uv_b = sprite.uv_end;

        let width = sprite.width;
        let height = sprite.height;

        let topleft = (
            -width / 2f32 - sprite.origin.0,
            -height / 2f32 - sprite.origin.1,
        );
        let topleft_uv = uv_a;

        let topright = (
            width / 2f32 - sprite.origin.0,
            -height / 2f32 - sprite.origin.1,
        );
        let topright_uv = (uv_b.0, uv_a.1);

        let bottomleft = (
            -width / 2f32 - sprite.origin.0,
            height / 2f32 - sprite.origin.1,
        );
        let bottomleft_uv = (uv_a.0, uv_b.1);

        let bottomright = (
            width / 2f32 - sprite.origin.0,
            height / 2f32 - sprite.origin.1,
        );
        let bottomright_uv = (uv_b.0, uv_b.1);

        let index = if let Some(value) = tex.removed.pop() {
            value as u32
        } else {
            let old = tex.count;
            tex.count += 1;
            old
        };

        unsafe {
            let idx = (index * 4 * 10 * 4) as usize;

            while tex.mockbuffer.len() <= idx {
                tex.mockbuffer.extend([0u8; 4 * 40].iter());
            }
            for (i, (point, uv)) in [
                (topleft, topleft_uv),
                (bottomleft, bottomleft_uv),
                (bottomright, bottomright_uv),
                (topright, topright_uv),
            ]
            .iter()
            .enumerate()
            {
                let idx = idx + i * 10 * 4;
                use std::mem::transmute;
                let x = &transmute::<f32, [u8; 4]>(point.0);
                let y = &transmute::<f32, [u8; 4]>(point.1);

                let uv0 = &transmute::<f32, [u8; 4]>(uv.0);
                let uv1 = &transmute::<f32, [u8; 4]>(uv.1);

                let tr0 = &transmute::<f32, [u8; 4]>(sprite.translation.0);
                let tr1 = &transmute::<f32, [u8; 4]>(sprite.translation.1);

                let rot = &transmute::<f32, [u8; 4]>(sprite.rotation);
                let scale = &transmute::<f32, [u8; 4]>(sprite.scale);

                let colors = &transmute::<(u8, u8, u8, u8), [u8; 4]>(sprite.colors[i]);

                tex.mockbuffer[idx..idx + 4].copy_from_slice(x);
                tex.mockbuffer[idx + 4..idx + 8].copy_from_slice(y);
                tex.mockbuffer[idx + 8..idx + 12]
                    .copy_from_slice(&transmute::<f32, [u8; 4]>(sprite.depth));

                tex.mockbuffer[idx + 12..idx + 16].copy_from_slice(uv0);
                tex.mockbuffer[idx + 16..idx + 20].copy_from_slice(uv1);

                tex.mockbuffer[idx + 20..idx + 24].copy_from_slice(tr0);
                tex.mockbuffer[idx + 24..idx + 28].copy_from_slice(tr1);

                tex.mockbuffer[idx + 28..idx + 32].copy_from_slice(rot);
                tex.mockbuffer[idx + 32..idx + 36].copy_from_slice(scale);
                tex.mockbuffer[idx + 36..idx + 40].copy_from_slice(colors);
            }
        }
        Handle(texture.0, index as usize)
    }

    /// Remove a texture
    ///
    /// Removes the texture from memory and destroys all sprites associated with it.
    /// All lingering sprite handles that were spawned using this texture handle will be
    /// invalidated.
    pub fn remove_layer(&mut self, texture: Layer) {
        let s = &mut *self.vx;
        let mut index = None;
        for (idx, x) in s.draw_order.iter().enumerate() {
            match x {
                DrawType::DynamicTexture { id } if *id == texture.0 => {
                    index = Some(idx);
                    break;
                }
                _ => {}
            }
        }
        if let Some(idx) = index {
            s.draw_order.remove(idx);
            // Can't delete here always because other textures may still be referring to later dyntexs,
            // only when this is the last texture.
            if s.dyntexs.len() == texture.0 + 1 {
                let dyntex = s.dyntexs.pop().unwrap();
                destroy_texture(s, dyntex);
            }
        }
    }

    /// Removes a single sprite, making it not be drawn
    pub fn remove_sprite(&mut self, handle: Handle) {
        let s = &mut *self.vx;
        if let Some(dyntex) = s.dyntexs.get_mut(handle.0) {
            let idx = (handle.1 * 4 * 10 * 4) as usize;
            let zero = unsafe { std::mem::transmute::<f32, [u8; 4]>(0.0) };
            for idx in (0..=3).map(|x| (x * 40) + idx) {
                dyntex.mockbuffer[idx + 32..idx + 36].copy_from_slice(&zero);
            }
            dyntex.removed.push(handle.1);
        }
    }

    /// Set the position of a sprite
    pub fn set_position(&mut self, handle: &Handle, position: (f32, f32)) {
        let s = &mut *self.vx;
        if let Some(stex) = s.dyntexs.get_mut(handle.0) {
            unsafe {
                use std::mem::transmute;
                let position0 = &transmute::<f32, [u8; 4]>(position.0);
                let position1 = &transmute::<f32, [u8; 4]>(position.1);

                let mut idx = (handle.1 * 4 * 10 * 4) as usize;

                stex.mockbuffer[idx + 5 * 4..idx + 6 * 4].copy_from_slice(position0);
                stex.mockbuffer[idx + 6 * 4..idx + 7 * 4].copy_from_slice(position1);
                idx += 40;
                stex.mockbuffer[idx + 5 * 4..idx + 6 * 4].copy_from_slice(position0);
                stex.mockbuffer[idx + 6 * 4..idx + 7 * 4].copy_from_slice(position1);
                idx += 40;
                stex.mockbuffer[idx + 5 * 4..idx + 6 * 4].copy_from_slice(position0);
                stex.mockbuffer[idx + 6 * 4..idx + 7 * 4].copy_from_slice(position1);
                idx += 40;
                stex.mockbuffer[idx + 5 * 4..idx + 6 * 4].copy_from_slice(position0);
                stex.mockbuffer[idx + 6 * 4..idx + 7 * 4].copy_from_slice(position1);
            }
        }
    }

    /// Set the rotation of a sprite
    ///
    /// Positive rotation goes counter-clockwise. The value of the rotation is in radians.
    pub fn set_rotation<T: Copy + Into<Rad<f32>>>(&mut self, handle: &Handle, rotation: T) {
        let s = &mut *self.vx;
        if let Some(stex) = s.dyntexs.get_mut(handle.0) {
            unsafe {
                use std::mem::transmute;
                let rot = &transmute::<f32, [u8; 4]>(rotation.into().0);

                let mut idx = (handle.1 * 4 * 10 * 4) as usize;

                stex.mockbuffer[idx + 7 * 4..idx + 8 * 4].copy_from_slice(rot);
                idx += 40;
                stex.mockbuffer[idx + 7 * 4..idx + 8 * 4].copy_from_slice(rot);
                idx += 40;
                stex.mockbuffer[idx + 7 * 4..idx + 8 * 4].copy_from_slice(rot);
                idx += 40;
                stex.mockbuffer[idx + 7 * 4..idx + 8 * 4].copy_from_slice(rot);
            }
        }
    }

    /// Translate all sprites that depend on a given texture
    ///
    /// Convenience method that translates all sprites associated with the given texture.
    pub fn sprite_translate_all(&mut self, tex: &Layer, dxdy: (f32, f32)) {
        let s = &mut *self.vx;
        if let Some(stex) = s.dyntexs.get_mut(tex.0) {
            unsafe {
                for mock in stex.mockbuffer.chunks_mut(40) {
                    use std::mem::transmute;
                    let x = transmute::<&[u8], &[f32]>(&mock[5 * 4..6 * 4]);
                    let y = transmute::<&[u8], &[f32]>(&mock[6 * 4..7 * 4]);
                    mock[5 * 4..6 * 4].copy_from_slice(&transmute::<f32, [u8; 4]>(x[0] + dxdy.0));
                    mock[6 * 4..7 * 4].copy_from_slice(&transmute::<f32, [u8; 4]>(y[0] + dxdy.1));
                }
            }
        }
    }

    /// Rotate all sprites that depend on a given texture
    ///
    /// Convenience method that rotates all sprites associated with the given texture.
    pub fn sprite_rotate_all<T: Copy + Into<Rad<f32>>>(&mut self, tex: &Layer, deg: T) {
        let s = &mut *self.vx;
        if let Some(stex) = s.dyntexs.get_mut(tex.0) {
            unsafe {
                for mock in stex.mockbuffer.chunks_mut(40) {
                    use std::mem::transmute;
                    let deggy = transmute::<&[u8], &[f32]>(&mock[28..32]);
                    mock[28..32]
                        .copy_from_slice(&transmute::<f32, [u8; 4]>(deggy[0] + deg.into().0));
                }
            }
        }
    }

    pub fn set_uv(&mut self, handle: &Handle, uv_begin: (f32, f32), uv_end: (f32, f32)) {
        let s = &mut *self.vx;
        if let Some(stex) = s.dyntexs.get_mut(handle.0) {
            if handle.1 < stex.count as usize {
                unsafe {
                    let mut idx = (handle.1 * 4 * 10 * 4) as usize;

                    use std::mem::transmute;
                    let begin0 = &transmute::<f32, [u8; 4]>(uv_begin.0);
                    let begin1 = &transmute::<f32, [u8; 4]>(uv_begin.1);
                    let end0 = &transmute::<f32, [u8; 4]>(uv_end.0);
                    let end1 = &transmute::<f32, [u8; 4]>(uv_end.1);

                    stex.mockbuffer[idx + 3 * 4..idx + 4 * 4].copy_from_slice(begin0);
                    stex.mockbuffer[idx + 4 * 4..idx + 5 * 4].copy_from_slice(begin1);
                    idx += 40;
                    stex.mockbuffer[idx + 3 * 4..idx + 4 * 4].copy_from_slice(begin0);
                    stex.mockbuffer[idx + 4 * 4..idx + 5 * 4].copy_from_slice(end1);
                    idx += 40;
                    stex.mockbuffer[idx + 3 * 4..idx + 4 * 4].copy_from_slice(end0);
                    stex.mockbuffer[idx + 4 * 4..idx + 5 * 4].copy_from_slice(end1);
                    idx += 40;
                    stex.mockbuffer[idx + 3 * 4..idx + 4 * 4].copy_from_slice(end0);
                    stex.mockbuffer[idx + 4 * 4..idx + 5 * 4].copy_from_slice(begin1);
                }
            }
        }
    }

    pub fn set_uvs<'b>(
        &mut self,
        mut uvs: impl Iterator<Item = (&'b Handle, (f32, f32), (f32, f32))>,
    ) {
        let s = &mut *self.vx;
        if let Some(first) = uvs.next() {
            if let Some(ref mut stex) = s.dyntexs.get_mut((first.0).0) {
                let current_texture_handle = (first.0).0;
                unsafe {
                    if (first.0).1 < stex.count as usize {
                        let mut idx = ((first.0).1 * 4 * 10 * 4) as usize;
                        let uv_begin = first.1;
                        let uv_end = first.2;

                        use std::mem::transmute;
                        let begin0 = &transmute::<f32, [u8; 4]>(uv_begin.0);
                        let begin1 = &transmute::<f32, [u8; 4]>(uv_begin.1);
                        let end0 = &transmute::<f32, [u8; 4]>(uv_end.0);
                        let end1 = &transmute::<f32, [u8; 4]>(uv_end.1);

                        stex.mockbuffer[idx + 3 * 4..idx + 4 * 4].copy_from_slice(begin0);
                        stex.mockbuffer[idx + 4 * 4..idx + 5 * 4].copy_from_slice(begin1);
                        idx += 40;
                        stex.mockbuffer[idx + 3 * 4..idx + 4 * 4].copy_from_slice(begin0);
                        stex.mockbuffer[idx + 4 * 4..idx + 5 * 4].copy_from_slice(end1);
                        idx += 40;
                        stex.mockbuffer[idx + 3 * 4..idx + 4 * 4].copy_from_slice(end0);
                        stex.mockbuffer[idx + 4 * 4..idx + 5 * 4].copy_from_slice(end1);
                        idx += 40;
                        stex.mockbuffer[idx + 3 * 4..idx + 4 * 4].copy_from_slice(end0);
                        stex.mockbuffer[idx + 4 * 4..idx + 5 * 4].copy_from_slice(begin1);
                    }
                    for handle in uvs {
                        if (handle.0).0 != current_texture_handle {
                            panic!["The texture handles of each sprite must be identical"];
                        }
                        if (handle.0).1 < stex.count as usize {
                            let mut idx = ((handle.0).1 * 4 * 10 * 4) as usize;
                            let uv_begin = handle.1;
                            let uv_end = handle.2;

                            use std::mem::transmute;
                            let begin0 = &transmute::<f32, [u8; 4]>(uv_begin.0);
                            let begin1 = &transmute::<f32, [u8; 4]>(uv_begin.1);
                            let end0 = &transmute::<f32, [u8; 4]>(uv_end.0);
                            let end1 = &transmute::<f32, [u8; 4]>(uv_end.1);

                            stex.mockbuffer[idx + 3 * 4..idx + 4 * 4].copy_from_slice(begin0);
                            stex.mockbuffer[idx + 4 * 4..idx + 5 * 4].copy_from_slice(begin1);
                            idx += 40;
                            stex.mockbuffer[idx + 3 * 4..idx + 4 * 4].copy_from_slice(begin0);
                            stex.mockbuffer[idx + 4 * 4..idx + 5 * 4].copy_from_slice(end1);
                            idx += 40;
                            stex.mockbuffer[idx + 3 * 4..idx + 4 * 4].copy_from_slice(end0);
                            stex.mockbuffer[idx + 4 * 4..idx + 5 * 4].copy_from_slice(end1);
                            idx += 40;
                            stex.mockbuffer[idx + 3 * 4..idx + 4 * 4].copy_from_slice(end0);
                            stex.mockbuffer[idx + 4 * 4..idx + 5 * 4].copy_from_slice(begin1);
                        }
                    }
                }
            }
        }
    }
}

// ---

fn destroy_texture(s: &mut VxDraw, mut dyntex: SingleTexture) {
    unsafe {
        dyntex.indices.destroy(&s.device);
        dyntex.texture_vertex_sprites.destroy(&s.device);
        s.device
            .destroy_image(ManuallyDrop::into_inner(read(&dyntex.texture_image_buffer)));
        s.device
            .free_memory(ManuallyDrop::into_inner(read(&dyntex.texture_image_memory)));
        s.device
            .destroy_render_pass(ManuallyDrop::into_inner(read(&dyntex.render_pass)));
        s.device
            .destroy_pipeline_layout(ManuallyDrop::into_inner(read(&dyntex.pipeline_layout)));
        s.device
            .destroy_graphics_pipeline(ManuallyDrop::into_inner(read(&dyntex.pipeline)));
        for dsl in dyntex.descriptor_set_layouts.drain(..) {
            s.device.destroy_descriptor_set_layout(dsl);
        }
        s.device
            .destroy_descriptor_pool(ManuallyDrop::into_inner(read(&dyntex.descriptor_pool)));
        s.device
            .destroy_sampler(ManuallyDrop::into_inner(read(&dyntex.sampler)));
        s.device
            .destroy_image_view(ManuallyDrop::into_inner(read(&dyntex.image_view)));
    }
}

// ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use cgmath::Deg;
    use logger::{Generic, GenericLogger, Logger};
    use rand::Rng;
    use rand_pcg::Pcg64Mcg as random;
    use std::f32::consts::PI;
    use test::Bencher;

    // ---

    static LOGO: &[u8] = include_bytes!["../images/logo.png"];
    static FOREST: &[u8] = include_bytes!["../images/forest-light.png"];
    static TESTURE: &[u8] = include_bytes!["../images/testure.png"];
    static TREE: &[u8] = include_bytes!["../images/treetest.png"];
    static FIREBALL: &[u8] = include_bytes!["../images/Fireball_68x9.png"];

    // ---

    #[test]
    fn overlapping_dyntex_respect_z_order() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let prspect = gen_perspective(&vx);

        let mut dyntex = vx.dyntex();

        let tree = dyntex.add_layer(TREE, LayerOptions::default());
        let logo = dyntex.add_layer(LOGO, LayerOptions::default());

        let sprite = Sprite {
            scale: 0.5,
            ..Sprite::default()
        };

        vx.dyntex().add(
            &tree,
            Sprite {
                depth: 0.5,
                ..sprite
            },
        );
        vx.dyntex().add(
            &logo,
            Sprite {
                depth: 0.6,
                translation: (0.25, 0.25),
                ..sprite
            },
        );

        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "overlapping_dyntex_respect_z_order", img);
    }

    #[test]
    fn simple_texture() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);

        let mut dyntex = vx.dyntex();
        let tex = dyntex.add_layer(LOGO, LayerOptions::default());
        vx.dyntex().add(&tex, Sprite::default());

        let prspect = gen_perspective(&vx);
        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "simple_texture", img);
    }

    #[test]
    fn simple_texture_adheres_to_view() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless2x1k);
        let tex = vx.dyntex().add_layer(LOGO, LayerOptions::default());
        vx.dyntex().add(&tex, Sprite::default());

        let prspect = gen_perspective(&vx);
        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "simple_texture_adheres_to_view", img);
    }

    #[test]
    fn colored_simple_texture() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let tex = vx.dyntex().add_layer(LOGO, LayerOptions::default());
        vx.dyntex().add(
            &tex,
            Sprite {
                colors: [
                    (255, 1, 2, 255),
                    (0, 255, 0, 255),
                    (0, 0, 255, 100),
                    (255, 2, 1, 0),
                ],
                ..Sprite::default()
            },
        );

        let prspect = gen_perspective(&vx);
        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "colored_simple_texture", img);
    }

    #[test]
    fn colored_simple_texture_set_position() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);

        let mut dyntex = vx.dyntex();
        let tex = dyntex.add_layer(LOGO, LayerOptions::default());
        let sprite = dyntex.add(
            &tex,
            Sprite {
                colors: [
                    (255, 1, 2, 255),
                    (0, 255, 0, 255),
                    (0, 0, 255, 100),
                    (255, 2, 1, 0),
                ],
                ..Sprite::default()
            },
        );
        dyntex.set_position(&sprite, (0.5, 0.3));

        let prspect = gen_perspective(&vx);
        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "colored_simple_texture_set_position", img);
    }

    #[test]
    fn translated_texture() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let tex = vx.dyntex().add_layer(
            LOGO,
            LayerOptions {
                depth_test: false,
                ..LayerOptions::default()
            },
        );

        let base = Sprite {
            width: 1.0,
            height: 1.0,
            ..Sprite::default()
        };

        let mut dyntex = vx.dyntex();

        dyntex.add(
            &tex,
            Sprite {
                translation: (-0.5, -0.5),
                rotation: 0.0,
                ..base
            },
        );
        dyntex.add(
            &tex,
            Sprite {
                translation: (0.5, -0.5),
                rotation: PI / 4.0,
                ..base
            },
        );
        dyntex.add(
            &tex,
            Sprite {
                translation: (-0.5, 0.5),
                rotation: PI / 2.0,
                ..base
            },
        );
        dyntex.add(
            &tex,
            Sprite {
                translation: (0.5, 0.5),
                rotation: PI,
                ..base
            },
        );
        dyntex.sprite_translate_all(&tex, (0.25, 0.35));

        let prspect = gen_perspective(&vx);
        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "translated_texture", img);
    }

    #[test]
    fn rotated_texture() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let mut dyntex = vx.dyntex();
        let tex = dyntex.add_layer(
            LOGO,
            LayerOptions {
                depth_test: false,
                ..LayerOptions::default()
            },
        );

        let base = Sprite {
            width: 1.0,
            height: 1.0,
            ..Sprite::default()
        };

        dyntex.add(
            &tex,
            Sprite {
                translation: (-0.5, -0.5),
                rotation: 0.0,
                ..base
            },
        );
        dyntex.add(
            &tex,
            Sprite {
                translation: (0.5, -0.5),
                rotation: PI / 4.0,
                ..base
            },
        );
        dyntex.add(
            &tex,
            Sprite {
                translation: (-0.5, 0.5),
                rotation: PI / 2.0,
                ..base
            },
        );
        dyntex.add(
            &tex,
            Sprite {
                translation: (0.5, 0.5),
                rotation: PI,
                ..base
            },
        );
        dyntex.sprite_rotate_all(&tex, Deg(90.0));

        let prspect = gen_perspective(&vx);
        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "rotated_texture", img);
    }

    #[test]
    fn many_sprites() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let tex = vx.dyntex().add_layer(
            LOGO,
            LayerOptions {
                depth_test: false,
                ..LayerOptions::default()
            },
        );
        for i in 0..360 {
            vx.dyntex().add(
                &tex,
                Sprite {
                    rotation: ((i * 10) as f32 / 180f32 * PI),
                    scale: 0.5,
                    ..Sprite::default()
                },
            );
        }

        let prspect = gen_perspective(&vx);
        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "many_sprites", img);
    }

    #[test]
    fn three_layer_scene() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let prspect = gen_perspective(&vx);

        let options = LayerOptions {
            depth_test: false,
            ..LayerOptions::default()
        };
        let mut dyntex = vx.dyntex();
        let forest = dyntex.add_layer(FOREST, options);
        let player = dyntex.add_layer(LOGO, options);
        let tree = dyntex.add_layer(TREE, options);

        vx.dyntex().add(&forest, Sprite::default());
        vx.dyntex().add(
            &player,
            Sprite {
                scale: 0.4,
                ..Sprite::default()
            },
        );
        vx.dyntex().add(
            &tree,
            Sprite {
                translation: (-0.3, 0.0),
                scale: 0.4,
                ..Sprite::default()
            },
        );

        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "three_layer_scene", img);
    }

    #[test]
    fn three_layer_scene_remove_middle() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let prspect = gen_perspective(&vx);

        let options = LayerOptions {
            depth_test: false,
            ..LayerOptions::default()
        };
        let mut dyntex = vx.dyntex();
        let forest = dyntex.add_layer(FOREST, options);
        let player = dyntex.add_layer(LOGO, options);
        let tree = dyntex.add_layer(TREE, options);

        dyntex.add(&forest, Sprite::default());
        let middle = dyntex.add(
            &player,
            Sprite {
                scale: 0.4,
                ..Sprite::default()
            },
        );
        dyntex.add(
            &tree,
            Sprite {
                translation: (-0.3, 0.0),
                scale: 0.4,
                ..Sprite::default()
            },
        );

        dyntex.remove_sprite(middle);

        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "three_layer_scene_remove_middle", img);
    }

    #[test]
    fn three_layer_scene_remove_middle_texture() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let prspect = gen_perspective(&vx);

        let options = LayerOptions {
            depth_test: false,
            ..LayerOptions::default()
        };
        let mut dyntex = vx.dyntex();
        let forest = dyntex.add_layer(FOREST, options);
        let player = dyntex.add_layer(LOGO, options);
        let tree = dyntex.add_layer(TREE, options);

        dyntex.add(&forest, Sprite::default());
        dyntex.add(
            &player,
            Sprite {
                scale: 0.4,
                ..Sprite::default()
            },
        );
        dyntex.add(
            &tree,
            Sprite {
                translation: (-0.3, 0.0),
                scale: 0.4,
                ..Sprite::default()
            },
        );

        dyntex.remove_layer(player);

        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "three_layer_scene_remove_middle_texture", img);

        vx.dyntex().remove_layer(tree);

        vx.draw_frame(&prspect);
    }

    #[test]
    fn three_layer_scene_remove_last_texture() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let prspect = gen_perspective(&vx);

        let options = LayerOptions {
            depth_test: false,
            ..LayerOptions::default()
        };

        let mut dyntex = vx.dyntex();
        let forest = dyntex.add_layer(FOREST, options);
        let player = dyntex.add_layer(LOGO, options);
        let tree = dyntex.add_layer(TREE, options);

        dyntex.add(&forest, Sprite::default());
        dyntex.add(
            &player,
            Sprite {
                scale: 0.4,
                ..Sprite::default()
            },
        );
        dyntex.add(
            &tree,
            Sprite {
                translation: (-0.3, 0.0),
                scale: 0.4,
                ..Sprite::default()
            },
        );

        dyntex.remove_layer(tree);

        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "three_layer_scene_remove_last_texture", img);

        vx.dyntex().remove_layer(player);

        vx.draw_frame(&prspect);
    }

    #[test]
    fn fixed_perspective() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless2x1k);
        let prspect = Matrix4::from_scale(0.0) * gen_perspective(&vx);

        let options = LayerOptions {
            depth_test: false,
            fixed_perspective: Some(Matrix4::identity()),
            ..LayerOptions::default()
        };
        let forest = vx.dyntex().add_layer(FOREST, options);

        vx.dyntex().add(&forest, Sprite::default());

        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "fixed_perspective", img);
    }

    #[test]
    fn change_of_uv_works_for_first() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let prspect = gen_perspective(&vx);

        let mut dyntex = vx.dyntex();

        let options = LayerOptions::default();
        let testure = dyntex.add_layer(TESTURE, options);
        let sprite = dyntex.add(&testure, Sprite::default());

        dyntex.set_uvs(std::iter::once((
            &sprite,
            (1.0 / 3.0, 0.0),
            (2.0 / 3.0, 1.0),
        )));

        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "change_of_uv_works_for_first", img);

        vx.dyntex()
            .set_uv(&sprite, (1.0 / 3.0, 0.0), (2.0 / 3.0, 1.0));

        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "change_of_uv_works_for_first", img);
    }

    #[test]
    fn set_single_sprite_rotation() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let prspect = gen_perspective(&vx);

        let mut dyntex = vx.dyntex();
        let options = LayerOptions::default();
        let testure = dyntex.add_layer(TESTURE, options);
        let sprite = dyntex.add(&testure, Sprite::default());
        dyntex.set_rotation(&sprite, Rad(0.3));

        let img = vx.draw_frame_copy_framebuffer(&prspect);
        utils::assert_swapchain_eq(&mut vx, "set_single_sprite_rotation", img);
    }

    #[test]
    fn push_and_pop_often_avoid_allocating_out_of_bounds() {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let prspect = gen_perspective(&vx);

        let options = LayerOptions::default();
        let testure = vx.dyntex().add_layer(TESTURE, options);

        let mut dyntex = vx.dyntex();
        for _ in 0..100_000 {
            let sprite = dyntex.add(&testure, Sprite::default());
            dyntex.remove_sprite(sprite);
        }

        vx.draw_frame(&prspect);
    }

    #[bench]
    fn bench_many_sprites(b: &mut Bencher) {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let tex = vx.dyntex().add_layer(LOGO, LayerOptions::default());
        for i in 0..1000 {
            vx.dyntex().add(
                &tex,
                Sprite {
                    rotation: ((i * 10) as f32 / 180f32 * PI),
                    scale: 0.5,
                    ..Sprite::default()
                },
            );
        }

        let prspect = gen_perspective(&vx);
        b.iter(|| {
            vx.draw_frame(&prspect);
        });
    }

    #[bench]
    fn bench_many_particles(b: &mut Bencher) {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let tex = vx.dyntex().add_layer(LOGO, LayerOptions::default());
        let mut rng = random::new(0);
        for i in 0..1000 {
            let (dx, dy) = (
                rng.gen_range(-1.0f32, 1.0f32),
                rng.gen_range(-1.0f32, 1.0f32),
            );
            vx.dyntex().add(
                &tex,
                Sprite {
                    translation: (dx, dy),
                    rotation: ((i * 10) as f32 / 180f32 * PI),
                    scale: 0.01,
                    ..Sprite::default()
                },
            );
        }

        let prspect = gen_perspective(&vx);
        b.iter(|| {
            vx.draw_frame(&prspect);
        });
    }

    #[bench]
    fn animated_fireballs_20x20_uvs2(b: &mut Bencher) {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let prspect = gen_perspective(&vx);

        let fireball_texture = vx.dyntex().add_layer(
            FIREBALL,
            LayerOptions {
                depth_test: false,
                ..LayerOptions::default()
            },
        );

        let mut fireballs = vec![];
        for idx in -10..10 {
            for jdx in -10..10 {
                fireballs.push(vx.dyntex().add(
                    &fireball_texture,
                    Sprite {
                        width: 0.68,
                        height: 0.09,
                        rotation: idx as f32 / 18.0 + jdx as f32 / 16.0,
                        translation: (idx as f32 / 10.0, jdx as f32 / 10.0),
                        ..Sprite::default()
                    },
                ));
            }
        }

        let width_elems = 10;
        let height_elems = 6;

        let mut counter = 0;

        b.iter(|| {
            let width_elem = counter % width_elems;
            let height_elem = counter / width_elems;
            let uv_begin = (
                width_elem as f32 / width_elems as f32,
                height_elem as f32 / height_elems as f32,
            );
            let uv_end = (
                (width_elem + 1) as f32 / width_elems as f32,
                (height_elem + 1) as f32 / height_elems as f32,
            );
            counter += 1;
            if counter > width_elems * height_elems {
                counter = 0;
            }

            vx.dyntex()
                .set_uvs(fireballs.iter().map(|id| (id, uv_begin, uv_end)));
            vx.draw_frame(&prspect);
        });
    }

    #[bench]
    fn bench_push_and_pop_sprite(b: &mut Bencher) {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);

        let options = LayerOptions::default();
        let testure = vx.dyntex().add_layer(TESTURE, options);

        let mut dyntex = vx.dyntex();
        b.iter(|| {
            let sprite = dyntex.add(&testure, Sprite::default());
            dyntex.remove_sprite(sprite);
        });
    }

    #[bench]
    fn bench_push_and_pop_texture(b: &mut Bencher) {
        let logger = Logger::<Generic>::spawn_void().to_logpass();
        let mut vx = VxDraw::new(logger, ShowWindow::Headless1k);
        let mut dyntex = vx.dyntex();

        b.iter(|| {
            let options = LayerOptions::default();
            let testure = dyntex.add_layer(TESTURE, options);
            dyntex.remove_layer(testure);
        });
    }
}
