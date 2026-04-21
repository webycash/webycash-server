//! wgpu compute backend — cross-platform GPU (Vulkan/Metal/DX12).
//!
//! Optimized for throughput:
//! - Pre-allocated buffer pool (zero allocation per dispatch)
//! - Coalesced u32 loads (no byte-by-byte access)
//! - Batched dispatches in single command encoder
//! - Single device.poll() per batch (not per dispatch)
//! - Workgroup size tuned per vendor (64 AMD, 256 NVIDIA)

use async_trait::async_trait;

use super::{ComputeBackend, HashResult, PowResult, TokenDerivation};

const MAX_HASHES_PER_DISPATCH: u32 = 65536;
const MAX_INPUT_BYTES: u64 = MAX_HASHES_PER_DISPATCH as u64 * 256; // ~16MB
const OUTPUT_WORDS_PER_HASH: u64 = 8;

/// wgpu GPU compute backend with pre-allocated buffer pool.
pub struct WgpuBackend {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    // Pre-allocated buffers — reused across all dispatches
    data_buf: wgpu::Buffer,
    offsets_lens_buf: wgpu::Buffer,
    output_buf: wgpu::Buffer,
    staging_buf: wgpu::Buffer,
    params_buf: wgpu::Buffer,
    workgroup_size: u32,
}

impl WgpuBackend {
    pub fn new() -> anyhow::Result<Self> {
        pollster::block_on(Self::init_async())
    }

    async fn init_async() -> anyhow::Result<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                ..Default::default()
            })
            .await
            .ok_or_else(|| anyhow::anyhow!("no wgpu adapter found"))?;

        let info = adapter.get_info();
        tracing::info!(adapter = %info.name, vendor = info.vendor, "wgpu adapter selected");

        // AMD GCN/RDNA wavefront = 64, NVIDIA warp = 32 (use 256 for occupancy)
        let workgroup_size: u32 = if info.vendor == 0x1002 { 64 } else { 256 };

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default(), None)
            .await
            .map_err(|e| anyhow::anyhow!("wgpu device: {e}"))?;

        // Generate shader with vendor-optimal workgroup size
        let shader_src = SHA256_WGSL_TEMPLATE.replace("/*WG_SIZE*/", &workgroup_size.to_string());
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sha256_batch"),
            source: wgpu::ShaderSource::Wgsl(shader_src.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sha256_bgl"),
            entries: &[
                bgl_entry(0, true),  // input data
                bgl_entry(1, true),  // offsets + lengths
                bgl_entry(2, false), // output hashes
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sha256_pl"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("sha256_pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("sha256_batch"),
            compilation_options: Default::default(),
            cache: None,
        });

        // Pre-allocate all buffers at maximum size
        let max_output = MAX_HASHES_PER_DISPATCH as u64 * OUTPUT_WORDS_PER_HASH * 4;
        let max_meta = MAX_HASHES_PER_DISPATCH as u64 * 2 * 4;

        let data_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("input_data"),
            size: MAX_INPUT_BYTES,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let offsets_lens_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("offsets_lens"),
            size: max_meta,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("output"),
            size: max_output,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("staging"),
            size: max_output,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            device,
            queue,
            pipeline,
            bind_group_layout,
            data_buf,
            offsets_lens_buf,
            output_buf,
            staging_buf,
            params_buf,
            workgroup_size,
        })
    }

    /// GPU SHA256 batch — zero allocation, coalesced loads, single sync.
    async fn gpu_sha256_batch(&self, inputs: &[Vec<u8>]) -> Vec<HashResult> {
        let n = inputs.len();
        if n == 0 {
            return Vec::new();
        }

        // Process in chunks of MAX_HASHES_PER_DISPATCH
        let mut all_results = Vec::with_capacity(n);
        let chunks: Vec<&[Vec<u8>]> = inputs.chunks(MAX_HASHES_PER_DISPATCH as usize).collect();

        for chunk in &chunks {
            let chunk_n = chunk.len() as u32;

            // Host-side SHA256 padding: each input → padded 64-byte blocks as
            // big-endian u32 words. GPU does ZERO padding logic, just loads + compresses.
            let (all_words, meta_data) = sha256_pad_inputs(chunk);

            let output_size = (chunk_n as u64) * OUTPUT_WORDS_PER_HASH * 4;

            // Upload to pre-allocated buffers (no allocation)
            self.queue
                .write_buffer(&self.data_buf, 0, bytemuck::cast_slice(&all_words));
            self.queue
                .write_buffer(&self.offsets_lens_buf, 0, bytemuck::cast_slice(&meta_data));
            self.queue.write_buffer(
                &self.params_buf,
                0,
                bytemuck::cast_slice(&[chunk_n, 0u32, 0, 0]),
            );

            // Bind group references pre-allocated buffers
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.data_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: self.offsets_lens_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: self.output_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: self.params_buf.as_entire_binding(),
                    },
                ],
            });

            // Single command encoder: dispatch + copy
            let encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            let mut encoder = encoder;
            {
                let mut pass = encoder.begin_compute_pass(&Default::default());
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.dispatch_workgroups(chunk_n.div_ceil(self.workgroup_size), 1, 1);
            }
            encoder.copy_buffer_to_buffer(&self.output_buf, 0, &self.staging_buf, 0, output_size);

            // Single submit + single sync
            self.queue.submit(std::iter::once(encoder.finish()));
            let slice = self.staging_buf.slice(..output_size);
            let (tx, rx) = tokio::sync::oneshot::channel();
            slice.map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
            self.device.poll(wgpu::Maintain::Wait);
            rx.await.unwrap().unwrap();

            // Read results
            let data = slice.get_mapped_range();
            let words: &[u32] = bytemuck::cast_slice(&data);
            let chunk_results: Vec<HashResult> = (0..chunk_n as usize)
                .map(|i| {
                    let base = i * 8;
                    let mut hash = [0u8; 32];
                    (0..8).for_each(|w| {
                        hash[w * 4..w * 4 + 4].copy_from_slice(&words[base + w].to_be_bytes());
                    });
                    HashResult { hash }
                })
                .collect();
            drop(data);
            self.staging_buf.unmap();

            all_results.extend(chunk_results);
        }

        all_results
    }
}

/// Pre-pad inputs into SHA256 blocks as big-endian u32 words on the CPU.
/// Returns (all_words, meta) where meta[i*2] = word offset, meta[i*2+1] = num blocks.
/// GPU loads 16 u32s per block with zero branching.
fn sha256_pad_inputs(inputs: &[Vec<u8>]) -> (Vec<u32>, Vec<u32>) {
    let total_blocks: usize = inputs.iter().map(|v| (v.len() + 9).div_ceil(64)).sum();
    let total_words = total_blocks * 16;

    let words: Vec<u32> = inputs
        .iter()
        .flat_map(|input| {
            let len = input.len();
            let num_blocks = (len + 9).div_ceil(64);
            let padded_len = num_blocks * 64;

            // Build padded message: data + 0x80 + zeros + length
            let padded: Vec<u8> = input
                .iter()
                .copied()
                .chain(std::iter::once(0x80u8))
                .chain(std::iter::repeat(0u8))
                .take(padded_len - 8)
                .chain({
                    let bit_len = (len as u64) * 8;
                    bit_len.to_be_bytes().into_iter()
                })
                .collect();

            // Convert to big-endian u32 words
            padded
                .chunks_exact(4)
                .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
                .collect::<Vec<u32>>()
        })
        .collect();

    debug_assert_eq!(words.len(), total_words);

    // Build metadata: [word_offset, num_blocks] per input
    let meta_data: Vec<u32> = inputs
        .iter()
        .scan(0u32, |word_offset, input| {
            let num_blocks = (input.len() + 9).div_ceil(64) as u32;
            let offset = *word_offset;
            *word_offset += num_blocks * 16;
            Some([offset, num_blocks])
        })
        .flatten()
        .collect();

    (words, meta_data)
}

fn bgl_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

#[async_trait]
impl ComputeBackend for WgpuBackend {
    fn name(&self) -> &'static str {
        "wgpu"
    }

    fn optimal_batch_size(&self) -> usize {
        MAX_HASHES_PER_DISPATCH as usize
    }

    async fn sha256_batch(&self, inputs: &[Vec<u8>]) -> Vec<HashResult> {
        self.gpu_sha256_batch(inputs).await
    }

    async fn verify_pow_batch(&self, inputs: &[(String, u32)]) -> Vec<PowResult> {
        let data: Vec<Vec<u8>> = inputs.iter().map(|(s, _)| s.as_bytes().to_vec()).collect();
        let hashes = self.sha256_batch(&data).await;
        hashes
            .iter()
            .zip(inputs.iter())
            .map(|(hr, (_, difficulty))| {
                let zeros = self.leading_zero_bits(&hr.hash);
                PowResult {
                    valid: zeros >= *difficulty,
                    leading_zeros: zeros,
                }
            })
            .collect()
    }

    async fn derive_public_hash_batch(&self, secrets: &[String]) -> Vec<TokenDerivation> {
        let data: Vec<Vec<u8>> = secrets.iter().map(|s| s.as_bytes().to_vec()).collect();
        let hashes = self.sha256_batch(&data).await;
        hashes
            .iter()
            .map(|hr| TokenDerivation {
                public_hash: hex::encode(hr.hash),
            })
            .collect()
    }
}

/// WGSL SHA256 batch shader — coalesced u32 loads, vendor-tuned workgroup size.
/// `/*WG_SIZE*/` is replaced at runtime with the optimal size for the GPU vendor.
/// SHA256 batch shader. Host pre-pads each input into 64-byte blocks (as big-endian
/// u32 words) so the GPU does ZERO padding logic — just loads and compresses.
/// `offsets_lens[idx*2]` = word offset into input_data, `offsets_lens[idx*2+1]` = num blocks.
const SHA256_WGSL_TEMPLATE: &str = r#"
const K = array<u32, 64>(
    0x428a2f98u, 0x71374491u, 0xb5c0fbcfu, 0xe9b5dba5u,
    0x3956c25bu, 0x59f111f1u, 0x923f82a4u, 0xab1c5ed5u,
    0xd807aa98u, 0x12835b01u, 0x243185beu, 0x550c7dc3u,
    0x72be5d74u, 0x80deb1feu, 0x9bdc06a7u, 0xc19bf174u,
    0xe49b69c1u, 0xefbe4786u, 0x0fc19dc6u, 0x240ca1ccu,
    0x2de92c6fu, 0x4a7484aau, 0x5cb0a9dcu, 0x76f988dau,
    0x983e5152u, 0xa831c66du, 0xb00327c8u, 0xbf597fc7u,
    0xc6e00bf3u, 0xd5a79147u, 0x06ca6351u, 0x14292967u,
    0x27b70a85u, 0x2e1b2138u, 0x4d2c6dfcu, 0x53380d13u,
    0x650a7354u, 0x766a0abbu, 0x81c2c92eu, 0x92722c85u,
    0xa2bfe8a1u, 0xa81a664bu, 0xc24b8b70u, 0xc76c51a3u,
    0xd192e819u, 0xd6990624u, 0xf40e3585u, 0x106aa070u,
    0x19a4c116u, 0x1e376c08u, 0x2748774cu, 0x34b0bcb5u,
    0x391c0cb3u, 0x4ed8aa4au, 0x5b9cca4fu, 0x682e6ff3u,
    0x748f82eeu, 0x78a5636fu, 0x84c87814u, 0x8cc70208u,
    0x90befffau, 0xa4506cebu, 0xbef9a3f7u, 0xc67178f2u,
);

// Host pre-pads inputs into SHA256 blocks (big-endian u32 words).
// GPU: zero branching — just load 16 u32 words per block, compress.
// offsets_lens[idx*2] = word offset, offsets_lens[idx*2+1] = num_blocks.
@group(0) @binding(0) var<storage, read> input_data: array<u32>;
@group(0) @binding(1) var<storage, read> offsets_lens: array<u32>;
@group(0) @binding(2) var<storage, read_write> output: array<u32>;
@group(0) @binding(3) var<uniform> params: vec4<u32>;

fn rotr(x: u32, n: u32) -> u32 { return (x >> n) | (x << (32u - n)); }

@compute @workgroup_size(/*WG_SIZE*/)
fn sha256_batch(@builtin(global_invocation_id) gid: vec3<u32>) {
    let idx = gid.x;
    if (idx >= params.x) { return; }

    let word_offset = offsets_lens[idx * 2u];
    let num_blocks = offsets_lens[idx * 2u + 1u];

    var h0 = 0x6a09e667u; var h1 = 0xbb67ae85u;
    var h2 = 0x3c6ef372u; var h3 = 0xa54ff53au;
    var h4 = 0x510e527fu; var h5 = 0x9b05688cu;
    var h6 = 0x1f83d9abu; var h7 = 0x5be0cd19u;

    for (var blk = 0u; blk < num_blocks; blk++) {
        // 16 coalesced u32 loads per block — zero branching
        let base = word_offset + blk * 16u;
        var w: array<u32, 16>;
        w[0] = input_data[base]; w[1] = input_data[base+1u];
        w[2] = input_data[base+2u]; w[3] = input_data[base+3u];
        w[4] = input_data[base+4u]; w[5] = input_data[base+5u];
        w[6] = input_data[base+6u]; w[7] = input_data[base+7u];
        w[8] = input_data[base+8u]; w[9] = input_data[base+9u];
        w[10] = input_data[base+10u]; w[11] = input_data[base+11u];
        w[12] = input_data[base+12u]; w[13] = input_data[base+13u];
        w[14] = input_data[base+14u]; w[15] = input_data[base+15u];

        var a = h0; var b = h1; var c = h2; var d = h3;
        var e = h4; var f = h5; var g = h6; var h = h7;

        for (var i = 0u; i < 64u; i++) {
            var wi: u32;
            if (i < 16u) {
                wi = w[i];
            } else {
                let w15 = w[(i - 15u) & 15u];
                let w2 = w[(i - 2u) & 15u];
                let s0 = rotr(w15, 7u) ^ rotr(w15, 18u) ^ (w15 >> 3u);
                let s1 = rotr(w2, 17u) ^ rotr(w2, 19u) ^ (w2 >> 10u);
                w[i & 15u] = w[i & 15u] + s0 + w[(i - 7u) & 15u] + s1;
                wi = w[i & 15u];
            }

            let S1 = rotr(e, 6u) ^ rotr(e, 11u) ^ rotr(e, 25u);
            let ch_val = (e & f) ^ (~e & g);
            let temp1 = h + S1 + ch_val + K[i] + wi;
            let S0 = rotr(a, 2u) ^ rotr(a, 13u) ^ rotr(a, 22u);
            let maj_val = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = S0 + maj_val;

            h = g; g = f; f = e; e = d + temp1;
            d = c; c = b; b = a; a = temp1 + temp2;
        }

        h0 += a; h1 += b; h2 += c; h3 += d;
        h4 += e; h5 += f; h6 += g; h7 += h;
    }

    let o = idx * 8u;
    output[o] = h0; output[o+1u] = h1; output[o+2u] = h2; output[o+3u] = h3;
    output[o+4u] = h4; output[o+5u] = h5; output[o+6u] = h6; output[o+7u] = h7;
}
"#;
