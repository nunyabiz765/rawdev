//! NLM on the graphics card (RTX 2060), through wgpu.
//!
//! **What a compute shader is.** A GPU runs one small program (a "shader") thousands of times at once,
//! each copy ("invocation") on a different piece of data. A *compute* shader is one that just crunches
//! numbers in buffers instead of drawing triangles. Ours is `nlm.wgsl`, written in WGSL (WebGPU's shading
//! language, Rust-ish syntax). Each invocation computes one output pixel; invocations are grouped into
//! 16 x 16 "workgroups" that share a small fast memory, so the neighbourhood they all need is read from
//! the card's main memory only once.
//!
//! **What wgpu is.** A Rust library that talks to the GPU through whatever the system has (here Vulkan,
//! via NVIDIA's driver). The flow is always the same:
//!   1. `Instance` -> pick an `Adapter` (a physical GPU) -> open a `Device` (our connection) + `Queue`;
//!   2. compile the shader into a `ComputePipeline`;
//!   3. create `Buffer`s (GPU memory) and upload the input;
//!   4. record commands ("bind these buffers, run N workgroups") into a `CommandEncoder`, submit them to
//!      the queue;
//!   5. copy the result into a buffer the CPU can map, wait, read it back.
//!
//! The maths is integer-only and identical to `nlm.rs` (same padded planes, same weight table, same
//! rounding), so the output is expected to match the CPU sample for sample; `--gpu` is checked against
//! the CPU path by `examples/nlm_bench.rs --check`.
//!
//! The work is split into horizontal bands (`BAND_ROWS` rows per dispatch) so no single GPU job runs
//! long: the desktop also draws on this card, and short jobs let it keep the screen responsive. Memory
//! use is small (about 0.45 GB peak for a 24 MP frame), so nothing else needs tiling on a 6 GB card.
//!
//! `RAWDEV_GPU_TIMING=1` prints, per call, how long the CPU-side preparation, the GPU work and the
//! readback took.

use crate::Result;
use crate::image::Gray8;
use crate::nlm;
use std::sync::OnceLock;
use std::sync::mpsc;
use wgpu::util::DeviceExt;

const SHADER: &str = include_str!("nlm.wgsl");
const BLOCK: usize = 16; // must match B in nlm.wgsl
const BAND_ROWS: usize = 256; // output rows per dispatch (16 workgroup rows)
const UNIFORM_STRIDE: u64 = 256; // dynamic uniform offsets must be 256-byte aligned

/// An open connection to the GPU plus the two compiled pipelines (1-channel and 2-channel NLM).
pub struct GpuNlm {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    pipelines: [wgpu::ComputePipeline; 2],
    /// e.g. "NVIDIA GeForce RTX 2060 (Vulkan)"
    pub name: String,
}

static GPU: OnceLock<Option<GpuNlm>> = OnceLock::new();

/// Turn the GPU path on for this process (`--gpu`). Returns the adapter name, or an error saying why
/// there is no usable GPU, in which case NLM keeps running on the CPU.
pub fn enable() -> Result<String> {
    enable_adapter(None)
}

/// [`enable`], choosing the adapter whose name contains `want` (case-insensitive), e.g. "2060",
/// "radeon", "llvmpipe". Naming an adapter is the only way to get a software (CPU) Vulkan device.
pub fn enable_adapter(want: Option<&str>) -> Result<String> {
    let gpu = GPU.get_or_init(|| match GpuNlm::new_with(want) {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("rawdev: --gpu: no usable GPU ({e}); NLM stays on the CPU");
            None
        }
    });
    gpu.as_ref()
        .map(|g| g.name.clone())
        .ok_or_else(|| "no usable GPU".into())
}

/// The GPU, if `enable()` was called and succeeded.
pub fn active() -> Option<&'static GpuNlm> {
    GPU.get().and_then(|g| g.as_ref())
}

/// One Vulkan adapter, for `rawdev gpus` / `rawdev check`.
pub struct AdapterInfo {
    pub name: String,
    /// "discrete", "integrated", "virtual", "cpu" or "other"
    pub kind: &'static str,
    pub driver: String,
}

fn instance() -> wgpu::Instance {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::VULKAN;
    wgpu::Instance::new(desc)
}

fn kind(t: wgpu::DeviceType) -> &'static str {
    match t {
        wgpu::DeviceType::DiscreteGpu => "discrete",
        wgpu::DeviceType::IntegratedGpu => "integrated",
        wgpu::DeviceType::VirtualGpu => "virtual",
        wgpu::DeviceType::Cpu => "cpu",
        _ => "other",
    }
}

/// Every Vulkan adapter on this machine.
pub fn list() -> Vec<AdapterInfo> {
    pollster::block_on(instance().enumerate_adapters(wgpu::Backends::VULKAN))
        .iter()
        .map(|a| {
            let i = a.get_info();
            AdapterInfo {
                name: i.name.clone(),
                kind: kind(i.device_type),
                driver: format!("{} {}", i.driver, i.driver_info).trim().to_string(),
            }
        })
        .collect()
}

impl GpuNlm {
    pub fn new() -> Result<GpuNlm> {
        GpuNlm::new_with(None)
    }

    /// Open the default high-performance Vulkan adapter, or the one whose name contains `want`.
    pub fn new_with(want: Option<&str>) -> Result<GpuNlm> {
        // Only Vulkan. A software (CPU) Vulkan device such as llvmpipe would be far slower than our own
        // CPU code, so it is refused unless asked for by name (useful for testing parity).
        let instance = instance();
        let adapter = match want {
            Some(w) => {
                let w = w.to_lowercase();
                let all = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
                let names: Vec<String> = all.iter().map(|a| a.get_info().name).collect();
                all.into_iter()
                    .find(|a| a.get_info().name.to_lowercase().contains(&w))
                    .ok_or_else(|| {
                        format!(
                            "no Vulkan adapter matching {w:?} (have: {})",
                            names.join(", ")
                        )
                    })?
            }
            None => pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                ..Default::default()
            }))?,
        };
        let info = adapter.get_info();
        if want.is_none() && info.device_type == wgpu::DeviceType::Cpu {
            return Err(format!("only a software adapter ({})", info.name).into());
        }
        // Ask for the card's real buffer limits (the defaults cap a binding at 128 MB).
        let limits = adapter.limits();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("rawdev"),
                required_limits: wgpu::Limits {
                    max_storage_buffer_binding_size: limits.max_storage_buffer_binding_size,
                    max_buffer_size: limits.max_buffer_size,
                    ..wgpu::Limits::default()
                },
                ..Default::default()
            }))?;

        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty,
            count: None,
        };
        let storage = |read_only| wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nlm"),
            entries: &[
                entry(
                    0,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: None,
                    },
                ),
                entry(1, storage(true)),
                entry(2, storage(true)),
                entry(3, storage(false)),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nlm"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipelines = [1, 2].map(|ch| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("nlm.wgsl"),
                source: wgpu::ShaderSource::Wgsl(
                    SHADER.replace("__CH__", &format!("{ch}u")).into(),
                ),
            });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("nlm"),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                cache: None,
            })
        });
        Ok(GpuNlm {
            device,
            queue,
            layout,
            pipelines,
            name: format!("{} ({:?})", info.name, info.backend),
        })
    }

    /// Same contract as `nlm::denoise::<C>` (C = 1 or 2), computed on the GPU.
    pub fn denoise<const C: usize>(&self, planes: [&Gray8; C], h: f32) -> Result<[Gray8; C]> {
        assert!(C == 1 || C == 2);
        let t0 = std::time::Instant::now();
        let (w, ht) = (planes[0].width, planes[0].height);
        let padded = planes.map(nlm::pad);
        let pw = padded[0].width;
        let ph = padded[0].data.len() / pw;
        let table = nlm::weight_table(h, C);

        // Upload the padded planes back to back, each rounded up to whole u32 words.
        let plane_words = padded[0].data.len().div_ceil(4);
        let mut bytes = vec![0u8; plane_words * 4 * C];
        for (c, p) in padded.iter().enumerate() {
            bytes[c * plane_words * 4..][..p.data.len()].copy_from_slice(&p.data);
        }
        drop(padded);
        let d = &self.device;
        let planes_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("planes"),
            contents: &bytes,
            usage: wgpu::BufferUsages::STORAGE,
        });
        drop(bytes);
        let table_bytes: Vec<u8> = table.iter().flat_map(|v| v.to_le_bytes()).collect();
        let table_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("weights"),
            contents: &table_bytes,
            usage: wgpu::BufferUsages::STORAGE,
        });
        // One Params block per band, 256 bytes apart; each dispatch picks its block by offset.
        let bands = ht.div_ceil(BAND_ROWS);
        let mut params = vec![0u8; bands * UNIFORM_STRIDE as usize];
        for b in 0..bands {
            let p: [u32; 8] = [
                pw as u32,
                ph as u32,
                w as u32,
                ht as u32,
                plane_words as u32,
                (table.len() - 1) as u32,
                (b * BAND_ROWS) as u32,
                0,
            ];
            for (k, v) in p.iter().enumerate() {
                params[b * UNIFORM_STRIDE as usize + 4 * k..][..4]
                    .copy_from_slice(&v.to_le_bytes());
            }
        }
        let params_buf = d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("params"),
            contents: &params,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let out_size = (w * ht * 4) as u64;
        let out_buf = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("out"),
            size: out_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let read_buf = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: out_size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nlm"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &params_buf,
                        offset: 0,
                        size: wgpu::BufferSize::new(32),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: planes_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: table_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: out_buf.as_entire_binding(),
                },
            ],
        });

        let t_prep = t0.elapsed().as_secs_f64();
        // One submission per band: short GPU jobs, so the desktop on the same card stays smooth.
        let groups_x = w.div_ceil(BLOCK) as u32;
        for b in 0..bands {
            let rows = BAND_ROWS.min(ht - b * BAND_ROWS);
            let mut enc = d.create_command_encoder(&Default::default());
            {
                let mut pass = enc.begin_compute_pass(&Default::default());
                pass.set_pipeline(&self.pipelines[C - 1]);
                pass.set_bind_group(0, &bind, &[(b as u64 * UNIFORM_STRIDE) as u32]);
                pass.dispatch_workgroups(groups_x, rows.div_ceil(BLOCK) as u32, 1);
            }
            if b + 1 == bands {
                enc.copy_buffer_to_buffer(&out_buf, 0, &read_buf, 0, out_size);
            }
            self.queue.submit([enc.finish()]);
        }

        // Wait for the GPU, then read the packed result back (byte c of each u32 = channel c).
        let (tx, rx) = mpsc::channel();
        read_buf
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        d.poll(wgpu::PollType::wait_indefinitely())?;
        rx.recv()??;
        let t_gpu = t0.elapsed().as_secs_f64();
        let out: [Gray8; C] = {
            let view = read_buf.slice(..).get_mapped_range()?;
            std::array::from_fn(|c| {
                let data = view.chunks_exact(4).map(|px| px[c]).collect();
                Gray8::from_vec(w, ht, data)
            })
        };
        read_buf.unmap();
        if std::env::var_os("RAWDEV_GPU_TIMING").is_some() {
            eprintln!(
                "gpu nlm C={C}: prep+upload {t_prep:.3}s, gpu {:.3}s, readback {:.3}s",
                t_gpu - t_prep,
                t0.elapsed().as_secs_f64() - t_gpu
            );
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencv::tests::{crop, prim};
    use crate::opencv::{self, Gamma};

    /// The GPU against cv2's own output (and so against the CPU path, which matches cv2 exactly).
    /// Needs the GPU and `tools/ref_primitives.py`'s files: `cargo test --release -- --ignored`.
    #[test]
    #[ignore]
    fn gpu_nlm_matches_cv2() {
        let g = GpuNlm::new().expect("no GPU");
        let c = crop();
        let [d] = g.denoise([&c.channel(1)], 6.0).unwrap();
        let bad = d
            .data
            .iter()
            .zip(prim("nlm_gray"))
            .filter(|(a, b)| *a != b)
            .count();
        assert_eq!(bad, 0, "gray NLM: {bad} pixels differ");

        let lab = opencv::rgb_to_lab(&c, Gamma::Linear);
        let [a, b] = g.denoise([&lab.channel(1), &lab.channel(2)], 9.0).unwrap();
        let want = prim("nlm_ab");
        let bad = (0..a.data.len())
            .filter(|&i| a.data[i] != want[2 * i] || b.data[i] != want[2 * i + 1])
            .count();
        assert_eq!(bad, 0, "ab NLM: {bad} pixels differ");
    }

    /// Odd sizes (not multiples of the 16-pixel block or of 4 bytes), strengths across the recipes'
    /// range: GPU == CPU sample for sample.
    #[test]
    #[ignore]
    fn gpu_nlm_matches_cpu_odd_sizes() {
        let g = GpuNlm::new().expect("no GPU");
        let mut seed = 12345u32;
        for (w, h) in [(37, 23), (301, 97), (17, 300)] {
            let mut noise = || {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 24) as u8
            };
            let p = Gray8::from_vec(
                w,
                h,
                (0..w * h)
                    .map(|i| ((i % w) as u8).wrapping_add(noise() / 8))
                    .collect(),
            );
            let q = Gray8::from_vec(w, h, (0..w * h).map(|_| noise()).collect());
            for hs in [3.0, 9.0, 21.0] {
                assert_eq!(
                    g.denoise([&p], hs).unwrap()[0].data,
                    nlm::denoise([&p], hs)[0].data
                );
                let [ga, gb] = g.denoise([&p, &q], hs).unwrap();
                let [ca, cb] = nlm::denoise([&p, &q], hs);
                assert!(ga.data == ca.data && gb.data == cb.data, "{w}x{h} h={hs}");
            }
        }
    }
}
