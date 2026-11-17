//! Neural speech check for Local Recordings (Silero VAD v6).
//!
//! The capture energy gate (`voisu_core::detect_speech`) passes steady room
//! noise, and Whisper-family Local models then invent "you" or "Thank you."
//! from it. Before a Local Recording reaches any Local model, this check runs
//! the Silero VAD network over it and refuses the Recording when no 32 ms
//! window sounds like speech. It only answers yes or no: the model still
//! receives the untouched Recording.
//!
//! The network is the faster-whisper 1.2.1 export of Silero VAD v6
//! (`faster_whisper/assets/silero_vad_v6.onnx`), embedded unmodified and
//! pinned by sha256. Only its weights are read from the ONNX file; the graph
//! (STFT, four Conv1d+ReLU, one LSTM cell, sigmoid decoder) is written out
//! below in plain Rust, so the daemon links no native inference runtime and
//! downloads nothing.
//!
//! Silero VAD: MIT, Copyright (c) 2020-present Silero Team. ONNX export:
//! faster-whisper, MIT, Copyright (c) 2023 SYSTRAN. See
//! `THIRD_PARTY_NOTICES.md`.

use std::sync::OnceLock;

/// The embedded Silero VAD v6 network, byte-for-byte faster-whisper's copy.
const SILERO_VAD_ONNX: &[u8] = include_bytes!("../assets/silero_vad_v6.onnx");

/// A Recording speaks when any window scores at least this. Silero's own
/// speech threshold is 0.5 and its release threshold 0.35; this sits well
/// below both because a wrong refusal loses words while a wrong pass only
/// lets the model's own no-text and hallucination filters decide. On 220
/// real captures (after the loudness gain below), the weakest clip Deepgram
/// transcribed peaked at 0.48 (0.39 when attenuated 30x), a Local "Okay"
/// clip at 0.28, and the loudest clip with no speech at 0.154. 0.2 sits
/// below the midpoint of that 0.154-0.28 gap, leaning toward accepting.
pub const SPEECH_THRESHOLD: f32 = 0.2;

/// Samples per Silero window at 16 kHz (32 ms).
const WINDOW: usize = 512;
/// Samples carried from the previous window into each one.
const CONTEXT: usize = 64;
const HIDDEN: usize = 128;
const FREQ_BINS: usize = 129;
const FFT_LEN: usize = 256;
const HOP: usize = 128;
/// Reflect padding on each side of the 576-sample context+window input.
const PAD: usize = 128;

/// 20 ms frames for the loudness estimate.
const LEVEL_FRAME: usize = 320;
/// Quiet Recordings are raised so their loud frames reach this RMS (int16
/// units). Silero scores quiet speech low; this keeps a low microphone gain
/// from turning speech into a refusal.
const TARGET_LEVEL: f64 = 1_000.0;
/// Upper bound on that raise, so near-digital silence is not blown up into
/// noise the network might call speech.
const MAX_GAIN: f64 = 16.0;

/// What the speech check concluded about one Recording.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpeechCheck {
    /// False only when every judged window scored below
    /// [`SPEECH_THRESHOLD`]. Too-short input and an unreadable model both
    /// count as speech, so the check never refuses what it could not judge.
    pub has_speech: bool,
    /// Highest window probability seen before the check stopped.
    pub peak_probability: f32,
    /// Windows scored. The check stops at the first speech window.
    pub windows_scored: usize,
    /// Loudness gain applied to the copy the network heard.
    pub gain: f32,
}

/// Judge a 16 kHz mono s16le Recording. A trailing odd byte and a trailing
/// partial window are ignored; input shorter than one window is accepted.
#[must_use]
pub fn check_speech(pcm_s16le: &[u8]) -> SpeechCheck {
    let samples: Vec<i16> = pcm_s16le
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect();
    check_samples(&samples)
}

/// [`check_speech`] over decoded samples.
#[must_use]
pub fn check_samples(samples: &[i16]) -> SpeechCheck {
    let gain = loudness_gain(samples);
    let accept_unjudged = SpeechCheck {
        has_speech: true,
        peak_probability: 0.0,
        windows_scored: 0,
        gain: gain as f32,
    };
    if samples.len() < WINDOW {
        return accept_unjudged;
    }
    let Some(model) = model() else {
        return accept_unjudged;
    };
    let audio = normalized(samples, gain);
    let mut state = State::new();
    let mut peak = 0.0f32;
    let mut scored = 0;
    for start in (0..=audio.len() - WINDOW).step_by(WINDOW) {
        let probability = model.step(&mut state, &audio, start);
        scored += 1;
        peak = peak.max(probability);
        if probability >= SPEECH_THRESHOLD {
            return SpeechCheck {
                has_speech: true,
                peak_probability: peak,
                windows_scored: scored,
                gain: gain as f32,
            };
        }
    }
    SpeechCheck {
        has_speech: false,
        peak_probability: peak,
        windows_scored: scored,
        gain: gain as f32,
    }
}

/// Raw per-window speech probabilities for samples already scaled to
/// [-1, 1], with no loudness gain and no early stop. For evaluation and
/// parity checks against the reference ONNX runtime.
#[must_use]
pub fn window_probabilities(audio: &[f32]) -> Vec<f32> {
    let Some(model) = model() else {
        return Vec::new();
    };
    let mut state = State::new();
    (0..audio.len() / WINDOW)
        .map(|index| model.step(&mut state, audio, index * WINDOW))
        .collect()
}

/// Gain that lifts the 95th-percentile 20 ms frame RMS (DC removed per
/// frame) to [`TARGET_LEVEL`], within 1..=[`MAX_GAIN`]. Never attenuates.
fn loudness_gain(samples: &[i16]) -> f64 {
    let mut levels: Vec<f64> = samples
        .as_chunks::<LEVEL_FRAME>()
        .0
        .iter()
        .map(|frame| {
            let mean = frame.iter().map(|&s| f64::from(s)).sum::<f64>() / LEVEL_FRAME as f64;
            let power = frame
                .iter()
                .map(|&s| (f64::from(s) - mean).powi(2))
                .sum::<f64>()
                / LEVEL_FRAME as f64;
            power.sqrt()
        })
        .collect();
    if levels.is_empty() {
        return 1.0;
    }
    levels.sort_by(f64::total_cmp);
    // Linear interpolation between closest ranks, as numpy's default.
    let rank = 0.95 * (levels.len() - 1) as f64;
    let low = rank.floor() as usize;
    let high = rank.ceil() as usize;
    let level = levels[low] + (levels[high] - levels[low]) * (rank - low as f64);
    (TARGET_LEVEL / level.max(1.0)).clamp(1.0, MAX_GAIN)
}

fn normalized(samples: &[i16], gain: f64) -> Vec<f32> {
    samples
        .iter()
        .map(|&s| {
            let scaled = (f64::from(s) * gain).round().clamp(-32_768.0, 32_767.0);
            (scaled / 32_768.0) as f32
        })
        .collect()
}

fn model() -> Option<&'static SileroModel> {
    static MODEL: OnceLock<Option<SileroModel>> = OnceLock::new();
    MODEL
        .get_or_init(|| match SileroModel::parse(SILERO_VAD_ONNX) {
            Ok(model) => Some(model),
            Err(error) => {
                eprintln!("Local speech check disabled: Silero VAD weights unreadable ({error})");
                None
            }
        })
        .as_ref()
}

struct State {
    h: [f32; HIDDEN],
    c: [f32; HIDDEN],
}

impl State {
    fn new() -> Self {
        Self {
            h: [0.0; HIDDEN],
            c: [0.0; HIDDEN],
        }
    }
}

/// One Conv1d layer, kernel 3, padding 1. Weights are stored `[out][3][in]`
/// (transposed from ONNX `[out][in][3]`) and activations time-major
/// `[time][channel]`, so every inner product runs over contiguous memory.
struct Conv {
    weight: Vec<f32>,
    bias: Vec<f32>,
    inputs: usize,
    outputs: usize,
    stride: usize,
}

impl Conv {
    /// `input` is `[len][inputs]`; returns ReLU output `[out_len][outputs]`.
    fn forward_relu(&self, input: &[f32], len: usize) -> (Vec<f32>, usize) {
        let out_len = (len + 2 - 3) / self.stride + 1;
        let mut out = vec![0.0f32; self.outputs * out_len];
        for t in 0..out_len {
            for o in 0..self.outputs {
                let mut acc = self.bias[o];
                for k in 0..3 {
                    // Padding 1: tap -1 and tap `len` read zeros.
                    let Some(tap) = (t * self.stride + k)
                        .checked_sub(1)
                        .filter(|&tap| tap < len)
                    else {
                        continue;
                    };
                    let kernel = (o * 3 + k) * self.inputs;
                    acc += dot(
                        &self.weight[kernel..kernel + self.inputs],
                        &input[tap * self.inputs..(tap + 1) * self.inputs],
                    );
                }
                out[t * self.outputs + o] = acc.max(0.0);
            }
        }
        (out, out_len)
    }
}

struct SileroModel {
    /// STFT basis `[258][256]`: rows 0..129 real, 129..258 imaginary.
    basis: Vec<f32>,
    convs: [Conv; 4],
    /// LSTM input weights `[512][128]`, gate order i, o, f, c (ONNX).
    w_input: Vec<f32>,
    /// LSTM recurrent weights `[512][128]`.
    w_recurrent: Vec<f32>,
    /// Input and recurrent biases summed, `[512]`.
    bias: Vec<f32>,
    decoder_weight: Vec<f32>,
    decoder_bias: f32,
}

impl SileroModel {
    fn parse(onnx: &[u8]) -> Result<Self, String> {
        let tensors = onnx::float_initializers(onnx)?;
        let take = |name: &str, dims: &[i64]| -> Result<Vec<f32>, String> {
            let (shape, values) = tensors
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, shape, values)| (shape, values))
                .ok_or_else(|| format!("missing initializer {name}"))?;
            if shape != dims {
                return Err(format!("{name} has shape {shape:?}, expected {dims:?}"));
            }
            Ok(values.clone())
        };
        let conv = |index: usize, outputs: usize, inputs: usize, stride: usize| {
            Ok::<_, String>(Conv {
                weight: transpose_kernel(
                    &take(
                        &format!("encoder.conv_layers.{index}.weight"),
                        &[outputs as i64, inputs as i64, 3],
                    )?,
                    outputs,
                    inputs,
                ),
                bias: take(
                    &format!("encoder.conv_layers.{index}.bias"),
                    &[outputs as i64],
                )?,
                inputs,
                outputs,
                stride,
            })
        };
        let lstm_bias = take("onnx::LSTM_211", &[1, 1024])?;
        Ok(Self {
            basis: take(
                "encoder.feature_extractor.forward_basis_buffer",
                &[258, 1, 256],
            )?,
            convs: [
                conv(0, 128, 129, 1)?,
                conv(1, 64, 128, 2)?,
                conv(2, 64, 64, 2)?,
                conv(3, 128, 64, 1)?,
            ],
            w_input: take("onnx::LSTM_209", &[1, 512, 128])?,
            w_recurrent: take("onnx::LSTM_210", &[1, 512, 128])?,
            bias: (0..512)
                .map(|g| lstm_bias[g] + lstm_bias[512 + g])
                .collect(),
            decoder_weight: take("decoder.conv1d.weight", &[1, 128, 1])?,
            decoder_bias: take("decoder.conv1d.bias", &[1])?[0],
        })
    }

    /// Score the window starting at `start` and advance the LSTM state.
    fn step(&self, state: &mut State, audio: &[f32], start: usize) -> f32 {
        // 64 context samples from the previous window (zeros first), then
        // the window itself, reflect-padded by 128 on each side.
        let mut input = [0.0f32; CONTEXT + WINDOW];
        if start >= CONTEXT {
            input[..CONTEXT].copy_from_slice(&audio[start - CONTEXT..start]);
        }
        input[CONTEXT..].copy_from_slice(&audio[start..start + WINDOW]);
        let n = input.len();
        let mut padded = [0.0f32; CONTEXT + WINDOW + 2 * PAD];
        for (i, slot) in padded.iter_mut().enumerate() {
            let j = i as isize - PAD as isize;
            let src = if j < 0 {
                -j
            } else if j as usize >= n {
                2 * (n as isize - 1) - j
            } else {
                j
            };
            *slot = input[src as usize];
        }

        // STFT magnitude. The export computes five frames and drops the first.
        let frames = (padded.len() - FFT_LEN) / HOP + 1;
        let used = frames - 1;
        let mut magnitude = vec![0.0f32; FREQ_BINS * used];
        for f in 0..used {
            let segment = &padded[(f + 1) * HOP..(f + 1) * HOP + FFT_LEN];
            for bin in 0..FREQ_BINS {
                let re = dot(&self.basis[bin * FFT_LEN..(bin + 1) * FFT_LEN], segment);
                let im = dot(
                    &self.basis[(FREQ_BINS + bin) * FFT_LEN..(FREQ_BINS + bin + 1) * FFT_LEN],
                    segment,
                );
                magnitude[f * FREQ_BINS + bin] = (re * re + im * im).sqrt();
            }
        }

        let mut features = magnitude;
        let mut len = used;
        for conv in &self.convs {
            let (next, next_len) = conv.forward_relu(&features, len);
            features = next;
            len = next_len;
        }
        debug_assert_eq!(len, 1);

        // One LSTM cell step, ONNX gate order i, o, f, c.
        let mut gates = [0.0f32; 4 * HIDDEN];
        for (g, gate) in gates.iter_mut().enumerate() {
            *gate = self.bias[g]
                + dot(&self.w_input[g * HIDDEN..(g + 1) * HIDDEN], &features)
                + dot(&self.w_recurrent[g * HIDDEN..(g + 1) * HIDDEN], &state.h);
        }
        for k in 0..HIDDEN {
            let input_gate = sigmoid(gates[k]);
            let output_gate = sigmoid(gates[HIDDEN + k]);
            let forget_gate = sigmoid(gates[2 * HIDDEN + k]);
            let cell = gates[3 * HIDDEN + k].tanh();
            state.c[k] = forget_gate * state.c[k] + input_gate * cell;
            state.h[k] = output_gate * state.c[k].tanh();
        }

        let logit = self.decoder_bias
            + self
                .decoder_weight
                .iter()
                .zip(state.h.iter())
                .map(|(w, h)| w * h.max(0.0))
                .sum::<f32>();
        sigmoid(logit)
    }
}

/// `[out][in][3]` to `[out][3][in]`.
fn transpose_kernel(weight: &[f32], outputs: usize, inputs: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; weight.len()];
    for o in 0..outputs {
        for i in 0..inputs {
            for k in 0..3 {
                out[(o * 3 + k) * inputs + i] = weight[(o * inputs + i) * 3 + k];
            }
        }
    }
    out
}

/// Eight independent sums so the compiler can vectorize; a single running
/// sum is a serial chain under IEEE ordering.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let (a_lanes, a_rest) = a.as_chunks::<8>();
    let (b_lanes, b_rest) = b.as_chunks::<8>();
    let mut sums = [0.0f32; 8];
    for (x, y) in a_lanes.iter().zip(b_lanes) {
        for lane in 0..8 {
            sums[lane] += x[lane] * y[lane];
        }
    }
    let tail: f32 = a_rest.iter().zip(b_rest).map(|(x, y)| x * y).sum();
    sums.iter().sum::<f32>() + tail
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// The few protobuf fields needed to read float initializers from an ONNX
/// `ModelProto`: graph (7) → initializer (5) → dims (1), data_type (2),
/// name (8), raw_data (9).
mod onnx {
    type Initializer = (String, Vec<i64>, Vec<f32>);

    pub(super) fn float_initializers(model: &[u8]) -> Result<Vec<Initializer>, String> {
        let mut out = Vec::new();
        for (field, value) in fields(model)? {
            if field != 7 {
                continue;
            }
            let Value::Bytes(graph) = value else {
                return Err("graph is not a message".into());
            };
            for (field, value) in fields(graph)? {
                if let (5, Value::Bytes(tensor)) = (field, value)
                    && let Some(initializer) = float_tensor(tensor)?
                {
                    out.push(initializer);
                }
            }
        }
        Ok(out)
    }

    /// A FLOAT tensor stored as `raw_data`; other tensors are skipped.
    fn float_tensor(tensor: &[u8]) -> Result<Option<Initializer>, String> {
        let mut dims = Vec::new();
        let mut data_type = 0;
        let mut name = String::new();
        let mut raw: Option<&[u8]> = None;
        for (field, value) in fields(tensor)? {
            match (field, value) {
                (1, Value::Varint(dim)) => dims.push(dim as i64),
                (1, Value::Bytes(packed)) => {
                    let mut at = 0;
                    while at < packed.len() {
                        dims.push(varint(packed, &mut at)? as i64);
                    }
                }
                (2, Value::Varint(kind)) => data_type = kind,
                (8, Value::Bytes(bytes)) => {
                    name = String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())?;
                }
                (9, Value::Bytes(bytes)) => raw = Some(bytes),
                _ => {}
            }
        }
        // TensorProto.DataType FLOAT = 1.
        let (1, Some(raw)) = (data_type, raw) else {
            return Ok(None);
        };
        if raw.len() % 4 != 0 {
            return Err(format!("{name} raw_data is not whole floats"));
        }
        let values: Vec<f32> = raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        let expected = dims
            .iter()
            .try_fold(1_i64, |product, &dim| product.checked_mul(dim))
            .ok_or_else(|| format!("{name} dims {dims:?} overflow"))?;
        if expected != values.len() as i64 {
            return Err(format!(
                "{name} has {} floats for dims {dims:?}",
                values.len()
            ));
        }
        Ok(Some((name, dims, values)))
    }

    enum Value<'a> {
        Varint(u64),
        Bytes(&'a [u8]),
        Fixed,
    }

    fn fields(message: &[u8]) -> Result<Vec<(u64, Value<'_>)>, String> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < message.len() {
            let key = varint(message, &mut at)?;
            let value = match key & 7 {
                0 => Value::Varint(varint(message, &mut at)?),
                1 => skip(message, &mut at, 8)?,
                2 => {
                    let len = usize::try_from(varint(message, &mut at)?)
                        .map_err(|_| "length overflow".to_owned())?;
                    let end = at.checked_add(len).filter(|&end| end <= message.len());
                    let end = end.ok_or("truncated field")?;
                    let bytes = &message[at..end];
                    at = end;
                    Value::Bytes(bytes)
                }
                5 => skip(message, &mut at, 4)?,
                wire => return Err(format!("unsupported wire type {wire}")),
            };
            out.push((key >> 3, value));
        }
        Ok(out)
    }

    fn skip<'a>(message: &[u8], at: &mut usize, len: usize) -> Result<Value<'a>, String> {
        if *at + len > message.len() {
            return Err("truncated fixed field".into());
        }
        *at += len;
        Ok(Value::Fixed)
    }

    fn varint(message: &[u8], at: &mut usize) -> Result<u64, String> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *message.get(*at).ok_or("truncated varint")?;
            *at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Ok(value);
            }
        }
        Err("varint too long".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    /// 3 s from whisper.cpp `samples/jfk.wav` (public domain: JFK's 1961
    /// inaugural address), 16 kHz mono s16le.
    const JFK_WAV: &[u8] = include_bytes!("../tests/fixtures/jfk-3s.wav");

    fn jfk_samples() -> Vec<i16> {
        // Canonical 44-byte header; the fixture is checked in as written.
        assert_eq!(&JFK_WAV[..4], b"RIFF");
        JFK_WAV[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect()
    }

    fn scaled(samples: &[i16], factor: f64) -> Vec<i16> {
        samples
            .iter()
            .map(|&s| (f64::from(s) * factor).round() as i16)
            .collect()
    }

    /// Deterministic uniform noise in [-amplitude, amplitude].
    fn noise(len: usize, amplitude: f64, mut seed: u64) -> Vec<i16> {
        (0..len)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let unit = (seed >> 33) as f64 / f64::from(1u32 << 31);
                ((unit * 2.0 - 1.0) * amplitude).round() as i16
            })
            .collect()
    }

    fn sine(len: usize, hz: f64, amplitude: f64) -> Vec<i16> {
        (0..len)
            .map(|n| {
                (amplitude * (2.0 * std::f64::consts::PI * hz * n as f64 / 16_000.0).sin()).round()
                    as i16
            })
            .collect()
    }

    #[test]
    fn embedded_model_is_faster_whispers_silero_v6() {
        let digest = Sha256::digest(SILERO_VAD_ONNX);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "4cbf549b8326f60f80f2536d9eefeb450a9abe83365a098031c89719f1be17d2"
        );
        assert!(model().is_some(), "embedded weights must parse");
    }

    #[test]
    fn matches_onnx_runtime_on_speech() {
        // onnxruntime 1.23 (faster-whisper venv) on the same fixture, raw
        // audio, zero state, state carried window to window: every window.
        let reference: [f32; 93] = [
            0.023829, 0.019030, 0.098604, 0.102932, 0.068931, 0.056102, 0.085032, 0.120822,
            0.099011, 0.074175, 0.616215, 0.788068, 0.787933, 0.760164, 0.750316, 0.795237,
            0.863461, 0.873309, 0.876148, 0.810174, 0.735714, 0.828934, 0.897008, 0.889406,
            0.874432, 0.849969, 0.841480, 0.817791, 0.811410, 0.802600, 0.806158, 0.846223,
            0.864623, 0.860145, 0.855120, 0.777617, 0.771985, 0.778178, 0.834353, 0.862345,
            0.829833, 0.738505, 0.723865, 0.846875, 0.838809, 0.852345, 0.837202, 0.810412,
            0.797756, 0.794398, 0.796365, 0.777622, 0.813705, 0.857974, 0.879564, 0.883810,
            0.869795, 0.865048, 0.859014, 0.832599, 0.811005, 0.809660, 0.862066, 0.870897,
            0.739685, 0.827550, 0.773016, 0.617598, 0.516529, 0.370322, 0.276992, 0.189874,
            0.144322, 0.129084, 0.080651, 0.071299, 0.075516, 0.066246, 0.058064, 0.055848,
            0.061769, 0.045266, 0.035851, 0.040070, 0.035062, 0.031688, 0.030026, 0.032139,
            0.028542, 0.027457, 0.036354, 0.024336, 0.021521,
        ];
        let audio: Vec<f32> = jfk_samples()
            .iter()
            .map(|&s| f32::from(s) / 32_768.0)
            .collect();
        let probabilities = window_probabilities(&audio);
        assert_eq!(probabilities.len(), reference.len());
        for (index, (got, want)) in probabilities.iter().zip(reference).enumerate() {
            assert!(
                (got - want).abs() < 1e-4,
                "window {index}: {got} vs onnxruntime {want}"
            );
        }
    }

    #[test]
    fn speech_passes_at_normal_and_quiet_levels() {
        let speech = jfk_samples();
        for factor in [1.0, 0.1, 0.03] {
            let check = check_samples(&scaled(&speech, factor));
            assert!(check.has_speech, "speech at x{factor} refused: {check:?}");
            assert!(check.peak_probability >= SPEECH_THRESHOLD);
            assert!(check.windows_scored < speech.len() / WINDOW, "stops early");
        }
    }

    #[test]
    fn noise_hum_tone_and_offset_are_refused() {
        let len = 3 * 16_000;
        let hum: Vec<i16> = sine(len, 50.0, 1_500.0)
            .iter()
            .zip(noise(len, 200.0, 3))
            .map(|(a, b)| a.saturating_add(b))
            .collect();
        // Synthetic white hiss is left out on purpose: after the loudness
        // gain about one 3 s clip in five has a window at 0.2 or above
        // (onnxruntime, 20 seeds per level), so it passes on to the model.
        // Real room-noise captures all stayed at or below 0.154.
        let cases = [
            ("mains hum", hum),
            ("1 kHz tone", sine(len, 1_000.0, 5_000.0)),
            ("DC offset", vec![3_000; len]),
            ("digital silence", vec![0; len]),
        ];
        for (name, samples) in cases {
            let check = check_samples(&samples);
            assert!(!check.has_speech, "{name} passed: {check:?}");
            assert_eq!(
                check.windows_scored,
                len / WINDOW,
                "{name} scans every window"
            );
        }
    }

    #[test]
    fn too_short_to_judge_is_accepted() {
        assert!(check_speech(&[]).has_speech);
        assert!(check_speech(&[1, 0, 2, 0]).has_speech);
        let almost_a_window = vec![0u8; 2 * (WINDOW - 1)];
        let check = check_speech(&almost_a_window);
        assert!(check.has_speech);
        assert_eq!(check.windows_scored, 0);
    }

    #[test]
    fn bytes_decode_as_little_endian_samples() {
        let speech = jfk_samples();
        let bytes: Vec<u8> = speech.iter().flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(check_speech(&bytes), check_samples(&speech));
    }

    #[test]
    fn loudness_gain_lifts_quiet_input_only() {
        assert_eq!(loudness_gain(&[]), 1.0);
        assert_eq!(loudness_gain(&noise(16_000, 20_000.0, 9)), 1.0);
        assert_eq!(loudness_gain(&vec![0; 16_000]), MAX_GAIN);
        let quiet = loudness_gain(&sine(16_000, 400.0, 200.0));
        // 400 Hz fills each 20 ms frame with whole cycles: RMS 200/sqrt(2).
        assert!((quiet - 1_000.0 / 141.42).abs() < 0.05, "{quiet}");
    }
}
