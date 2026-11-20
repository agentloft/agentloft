//! The transformer itself, for Qwen3 and Gemma 3: both are decoders with
//! grouped-query attention, per-head norms of queries and keys, and rotary
//! positions. Weights stay quantized as the GGUF file has them, the
//! embedding table included (it is also the output layer when the model
//! ties them); activations are f32.
//!
//! Gemma 3 adds norms after attention and after the feed-forward block, a
//! GELU feed-forward, scaled embeddings, and layers that only look back a
//! fixed window (five of every six), with rotary bases of their own.

use std::borrow::Cow;
use std::io::{Read, Seek};
use std::sync::Arc;

use anyhow::{Context as _, bail};
use candle_core::quantized::{QMatMul, QStorage, QTensor, gguf_file};
use candle_core::{D, DType, Device, Module as _, Tensor};

use crate::Arch;

/// The longest prompt and answer together, in tokens. Dictated text is
/// short; this bounds the rotary tables and the cache.
pub(crate) const CONTEXT: usize = 8192;

pub(crate) struct Decoder {
    embed: Arc<QTensor>,
    embed_scale: Option<f64>,
    layers: Vec<Layer>,
    norm: RmsNorm,
    output: QMatMul,
    /// Rotary tables: `[0]` for full-attention layers, `[1]` for windowed
    /// ones (the same as `[0]` for Qwen3).
    ropes: [Rope; 2],
    device: Device,
}

struct Layer {
    attn_norm: RmsNorm,
    q: QMatMul,
    k: QMatMul,
    v: QMatMul,
    o: QMatMul,
    q_norm: RmsNorm,
    k_norm: RmsNorm,
    post_attn_norm: Option<RmsNorm>,
    ffn_norm: RmsNorm,
    gate: QMatMul,
    up: QMatMul,
    down: QMatMul,
    post_ffn_norm: Option<RmsNorm>,
    gelu: bool,
    n_head: usize,
    n_kv_head: usize,
    head_dim: usize,
    /// How many positions back (itself included) it may look, if limited.
    window: Option<usize>,
    rope: usize,
    cache: Option<(Tensor, Tensor)>,
}

struct RmsNorm {
    weight: Tensor,
    eps: f32,
}

#[derive(Clone)]
struct Rope {
    cos: Tensor,
    sin: Tensor,
}

/// Reads tensors and settings out of a GGUF file.
///
/// Each tensor passes through one buffer, mapped for the purpose rather than
/// allocated: the system allocator keeps blocks this big for reuse once
/// they are freed (on macOS, gigabytes of them after a 4B model, with no
/// call that gives them back), while a mapping goes back to the system the
/// moment it is dropped.
struct Gguf<'a, R> {
    content: gguf_file::Content,
    reader: &'a mut R,
    device: &'a Device,
    prefix: &'static str,
    buffer: Option<memmap2::MmapMut>,
}

impl<R: Read + Seek> Gguf<'_, R> {
    fn get(&self, key: &str) -> anyhow::Result<&gguf_file::Value> {
        let key = format!("{}.{key}", self.prefix);
        self.content
            .metadata
            .get(&key)
            .with_context(|| format!("the model file has no {key}"))
    }

    fn usize(&self, key: &str) -> anyhow::Result<usize> {
        Ok(self.get(key)?.to_u32()? as usize)
    }

    fn f64(&self, key: &str) -> anyhow::Result<f64> {
        Ok(f64::from(self.get(key)?.to_f32()?))
    }

    fn has(&self, name: &str) -> bool {
        self.content.tensor_infos.contains_key(name)
    }

    fn tensor(&mut self, name: &str) -> anyhow::Result<QTensor> {
        self.read_tensor(name)
            .with_context(|| format!("couldn't read {name}"))
    }

    fn read_tensor(&mut self, name: &str) -> anyhow::Result<QTensor> {
        let info = self
            .content
            .tensor_infos
            .get(name)
            .context("the model file doesn't have it")?;
        let size = byte_size(info)?;
        let start = self
            .content
            .tensor_data_offset
            .checked_add(info.offset)
            .context("its offset is out of range")?;
        if self.buffer.as_ref().is_none_or(|b| b.len() < size) {
            // The largest tensor, so that this happens once.
            let largest = self
                .content
                .tensor_infos
                .values()
                .map(byte_size)
                .collect::<anyhow::Result<Vec<_>>>()?
                .into_iter()
                .max()
                .unwrap_or(0);
            self.buffer = None;
            self.buffer = Some(memmap2::MmapMut::map_anon(largest.max(1))?);
        }
        let buffer = &mut self.buffer.as_mut().context("no buffer")?[..size];
        self.reader.seek(std::io::SeekFrom::Start(start))?;
        self.reader.read_exact(buffer)?;
        let storage = QStorage::from_data(Cow::Borrowed(buffer), self.device, info.ggml_dtype)?;
        Ok(QTensor::new(storage, info.shape.clone())?)
    }

    fn matmul(&mut self, name: &str) -> anyhow::Result<QMatMul> {
        Ok(QMatMul::from_qtensor(self.tensor(name)?)?)
    }

    fn norm(&mut self, name: &str, eps: f64) -> anyhow::Result<RmsNorm> {
        Ok(RmsNorm {
            weight: self.tensor(name)?.dequantize(self.device)?,
            eps: eps as f32,
        })
    }
}

/// How many bytes a tensor takes in the file.
fn byte_size(info: &gguf_file::TensorInfo) -> anyhow::Result<usize> {
    let elems = info.shape.elem_count();
    let block = info.ggml_dtype.block_size();
    if !elems.is_multiple_of(block) {
        bail!("{elems} values don't make whole blocks of {block}");
    }
    Ok(elems / block * info.ggml_dtype.type_size())
}

impl Decoder {
    pub(crate) fn load<R: Read + Seek>(
        arch: Arch,
        reader: &mut R,
        device: &Device,
    ) -> anyhow::Result<Self> {
        let content = gguf_file::Content::read(reader).context("not a GGUF file")?;
        let prefix = match arch {
            Arch::Qwen3 => "qwen3",
            Arch::Gemma3 => "gemma3",
        };
        let found = content
            .metadata
            .get("general.architecture")
            .and_then(|v| v.to_string().ok())
            .cloned()
            .unwrap_or_default();
        if found != prefix {
            bail!("the model file is for {found:?}, not {prefix}");
        }
        let mut g = Gguf {
            content,
            reader,
            device,
            prefix,
            buffer: None,
        };
        let n_layer = g.usize("block_count")?;
        let n_head = g.usize("attention.head_count")?;
        let n_kv_head = g.usize("attention.head_count_kv")?;
        let head_dim = g.usize("attention.key_length")?;
        let hidden = g.usize("embedding_length")?;
        let eps = g.f64("attention.layer_norm_rms_epsilon")?;
        let base = g.f64("rope.freq_base")?;
        if n_kv_head == 0 || n_head % n_kv_head != 0 {
            bail!("{n_head} attention heads can't share {n_kv_head} key-value heads");
        }

        let gemma = arch == Arch::Gemma3;
        let (window, ropes) = if gemma {
            let window = g.usize("attention.sliding_window")?;
            // Linear position scaling, on full-attention layers only.
            let scale = g.f64("rope.scaling.factor").unwrap_or(1.0);
            let local_base = g.f64("rope.local_freq_base").unwrap_or(10_000.0);
            (
                Some(window),
                [
                    Rope::new(head_dim, base, scale, device)?,
                    Rope::new(head_dim, local_base, 1.0, device)?,
                ],
            )
        } else {
            let rope = Rope::new(head_dim, base, 1.0, device)?;
            (None, [rope.clone(), rope])
        };

        let embed = Arc::new(g.tensor("token_embd.weight")?);
        let mut layers = Vec::with_capacity(n_layer);
        for i in 0..n_layer {
            let p = format!("blk.{i}");
            // Gemma 3: every sixth layer sees everything, the rest a window.
            let windowed = gemma && (i + 1) % 6 != 0;
            layers.push(Layer {
                attn_norm: g.norm(&format!("{p}.attn_norm.weight"), eps)?,
                q: g.matmul(&format!("{p}.attn_q.weight"))?,
                k: g.matmul(&format!("{p}.attn_k.weight"))?,
                v: g.matmul(&format!("{p}.attn_v.weight"))?,
                o: g.matmul(&format!("{p}.attn_output.weight"))?,
                q_norm: g.norm(&format!("{p}.attn_q_norm.weight"), eps)?,
                k_norm: g.norm(&format!("{p}.attn_k_norm.weight"), eps)?,
                post_attn_norm: if gemma {
                    Some(g.norm(&format!("{p}.post_attention_norm.weight"), eps)?)
                } else {
                    None
                },
                ffn_norm: g.norm(&format!("{p}.ffn_norm.weight"), eps)?,
                gate: g.matmul(&format!("{p}.ffn_gate.weight"))?,
                up: g.matmul(&format!("{p}.ffn_up.weight"))?,
                down: g.matmul(&format!("{p}.ffn_down.weight"))?,
                post_ffn_norm: if gemma {
                    Some(g.norm(&format!("{p}.post_ffw_norm.weight"), eps)?)
                } else {
                    None
                },
                gelu: gemma,
                n_head,
                n_kv_head,
                head_dim,
                window: if windowed { window } else { None },
                rope: usize::from(windowed),
                cache: None,
            });
        }
        let norm = g.norm("output_norm.weight", eps)?;
        let output = if g.has("output.weight") {
            g.matmul("output.weight")?
        } else {
            QMatMul::from_arc(embed.clone())?
        };
        Ok(Self {
            embed,
            embed_scale: gemma.then(|| (hidden as f64).sqrt()),
            layers,
            norm,
            output,
            ropes,
            device: device.clone(),
        })
    }

    /// Forgets every position seen.
    pub(crate) fn clear(&mut self) {
        for layer in &mut self.layers {
            layer.cache = None;
        }
    }

    /// Runs `tokens`, which follow `offset` positions already seen, and
    /// returns the logits for the next token after the last one.
    pub(crate) fn forward(&mut self, tokens: &[u32], offset: usize) -> anyhow::Result<Tensor> {
        let t = tokens.len();
        if t == 0 {
            bail!("nothing to run");
        }
        if offset + t > CONTEXT {
            bail!("longer than the model's {CONTEXT} tokens");
        }
        let ids = Tensor::new(tokens, &self.device)?;
        let mut x = self.embed.embedding(&ids)?.unsqueeze(0)?;
        if let Some(scale) = self.embed_scale {
            x = (x * scale)?;
        }
        for layer in &mut self.layers {
            x = layer.forward(&x, offset, &self.ropes[layer.rope], &self.device)?;
        }
        let last = x.narrow(1, t - 1, 1)?;
        let last = self.norm.forward(&last)?;
        Ok(self.output.forward(&last)?.flatten_all()?)
    }
}

impl Layer {
    fn forward(
        &mut self,
        x: &Tensor,
        offset: usize,
        rope: &Rope,
        device: &Device,
    ) -> anyhow::Result<Tensor> {
        let h = self.attn_norm.forward(x)?;
        let mut h = self.attention(&h, offset, rope, device)?;
        if let Some(norm) = &self.post_attn_norm {
            h = norm.forward(&h)?;
        }
        let x = (x + h)?;
        let h = self.ffn_norm.forward(&x)?;
        let gate = self.gate.forward(&h)?;
        let gate = if self.gelu {
            gate.gelu()?
        } else {
            gate.silu()?
        };
        let mut h = self.down.forward(&(gate * self.up.forward(&h)?)?)?;
        if let Some(norm) = &self.post_ffn_norm {
            h = norm.forward(&h)?;
        }
        Ok((x + h)?)
    }

    fn attention(
        &mut self,
        x: &Tensor,
        offset: usize,
        rope: &Rope,
        device: &Device,
    ) -> anyhow::Result<Tensor> {
        let (b, t, _) = x.dims3()?;
        let (nh, nkv, hd) = (self.n_head, self.n_kv_head, self.head_dim);
        let heads = |w: &QMatMul, n: usize| -> candle_core::Result<Tensor> {
            w.forward(x)?
                .reshape((b, t, n, hd))?
                .transpose(1, 2)?
                .contiguous()
        };
        let q = self.q_norm.forward(&heads(&self.q, nh)?)?;
        let k = self.k_norm.forward(&heads(&self.k, nkv)?)?;
        let v = heads(&self.v, nkv)?;
        let (q, k) = rope.apply(&q, &k, offset)?;

        let (k, v) = match self.cache.take() {
            Some((ck, cv)) => (Tensor::cat(&[&ck, &k], 2)?, Tensor::cat(&[&cv, &v], 2)?),
            None => (k, v),
        };
        let s = k.dim(2)?;
        // Queries grouped by the key-value head they share: one matmul per
        // shared head instead of copying keys and values for every head.
        let groups = nh / nkv;
        let q = q.reshape((b, nkv, groups * t, hd))?;
        let scores = (q.matmul(&k.t()?.contiguous()?)? / (hd as f64).sqrt())?;
        let mut scores = scores.reshape((b, nh, t, s))?;
        if t > 1 {
            let mask = mask(t, s, offset, self.window, device)?;
            scores = scores.broadcast_add(&mask)?;
        }
        let probs = candle_nn::ops::softmax_last_dim(&scores.contiguous()?)?;
        let out = probs
            .reshape((b, nkv, groups * t, s))?
            .matmul(&v)?
            .reshape((b, nh, t, hd))?
            .transpose(1, 2)?
            .reshape((b, t, nh * hd))?;

        // A windowed layer keeps what the next position may still see: its
        // window, less the place of that position itself.
        let keep = |x: Tensor| -> candle_core::Result<Tensor> {
            match self.window {
                Some(w) if s >= w => x.narrow(2, s + 1 - w, w - 1)?.contiguous(),
                _ => Ok(x),
            }
        };
        self.cache = Some((keep(k)?, keep(v)?));
        Ok(self.o.forward(&out)?)
    }
}

/// What `t` new positions (after `offset`) may see of the `s` cached ones,
/// the last of which are the new ones: 0 where allowed, -inf elsewhere.
fn mask(
    t: usize,
    s: usize,
    offset: usize,
    window: Option<usize>,
    device: &Device,
) -> candle_core::Result<Tensor> {
    let first_key = offset + t - s;
    let values: Vec<f32> = (0..t)
        .flat_map(|i| {
            let query = offset + i;
            (0..s).map(move |j| {
                let key = first_key + j;
                let seen = key <= query && window.is_none_or(|w| query - key < w);
                if seen { 0.0 } else { f32::NEG_INFINITY }
            })
        })
        .collect();
    Tensor::from_vec(values, (t, s), device)
}

impl RmsNorm {
    fn forward(&self, x: &Tensor) -> candle_core::Result<Tensor> {
        candle_nn::ops::rms_norm(&x.contiguous()?, &self.weight, self.eps)
    }
}

impl Rope {
    fn new(head_dim: usize, base: f64, scale: f64, device: &Device) -> candle_core::Result<Self> {
        let inv_freq: Vec<f32> = (0..head_dim / 2)
            .map(|i| (1.0 / base.powf(2.0 * i as f64 / head_dim as f64) / scale) as f32)
            .collect();
        let inv_freq = Tensor::from_vec(inv_freq, (1, head_dim / 2), device)?;
        let positions = Tensor::arange(0u32, CONTEXT as u32, device)?
            .to_dtype(DType::F32)?
            .reshape((CONTEXT, 1))?;
        let angles = positions.matmul(&inv_freq)?;
        Ok(Self {
            cos: angles.cos()?,
            sin: angles.sin()?,
        })
    }

    fn apply(
        &self,
        q: &Tensor,
        k: &Tensor,
        offset: usize,
    ) -> candle_core::Result<(Tensor, Tensor)> {
        let t = q.dim(D::Minus2)?;
        let cos = self.cos.narrow(0, offset, t)?;
        let sin = self.sin.narrow(0, offset, t)?;
        Ok((
            candle_nn::rotary_emb::rope(q, &cos, &sin)?,
            candle_nn::rotary_emb::rope(k, &cos, &sin)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(m: &Tensor) -> Vec<Vec<bool>> {
        m.to_vec2::<f32>()
            .unwrap()
            .into_iter()
            .map(|row| row.into_iter().map(|v| v == 0.0).collect())
            .collect()
    }

    #[test]
    fn causal_mask() {
        let m = mask(3, 3, 0, None, &Device::Cpu).unwrap();
        assert_eq!(
            seen(&m),
            [
                [true, false, false],
                [true, true, false],
                [true, true, true]
            ]
        );
    }

    #[test]
    fn windowed_mask() {
        // A window of two: itself and the one before.
        let m = mask(4, 4, 0, Some(2), &Device::Cpu).unwrap();
        assert_eq!(
            seen(&m),
            [
                [true, false, false, false],
                [true, true, false, false],
                [false, true, true, false],
                [false, false, true, true]
            ]
        );
    }

    #[test]
    fn mask_after_cached_positions() {
        // Two new positions after three cached ones.
        let m = mask(2, 5, 3, None, &Device::Cpu).unwrap();
        assert_eq!(
            seen(&m),
            [
                [true, true, true, true, false],
                [true, true, true, true, true]
            ]
        );
    }
}
