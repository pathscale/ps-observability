//! Decode and report completed page-owned GPUBuffer writes.

use std::sync::Arc;

use base64::Engine as _;
use blitz_control_protocol::{DebugResponse, DiagnosticsRequest, WebGpuBufferSnapshot};
use eyre::{Result, bail, eyre};

use crate::inspector::{Client, is_pending};

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[serde(alias = "U32")]
    U32,
    #[serde(alias = "I32")]
    I32,
    #[serde(alias = "F32")]
    F32,
    #[serde(alias = "U8")]
    U8,
}

impl Format {
    pub fn width(self) -> usize {
        if self == Self::U8 { 1 } else { 4 }
    }

    fn number(self, bytes: &[u8]) -> f64 {
        if self == Self::U8 {
            return f64::from(bytes[0]);
        }
        let word = [bytes[0], bytes[1], bytes[2], bytes[3]];
        match self {
            Self::U32 => f64::from(u32::from_le_bytes(word)),
            Self::I32 => f64::from(i32::from_le_bytes(word)),
            Self::F32 => f64::from(f32::from_le_bytes(word)),
            Self::U8 => unreachable!("one-byte values returned above"),
        }
    }

    fn text(self, bytes: &[u8]) -> String {
        if self == Self::F32 {
            f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]).to_string()
        } else {
            self.number(bytes).to_string()
        }
    }

    /// Compare f32 data with bounds rounded to its own representation.
    pub fn bound(self, value: f64) -> std::result::Result<f64, String> {
        if !value.is_finite() {
            return Err("GPUBuffer bounds must be finite".to_owned());
        }
        if self == Self::F32 {
            let rounded = value as f32;
            if !rounded.is_finite() {
                return Err(format!("GPUBuffer bound {value} is outside finite f32"));
            }
            Ok(f64::from(rounded))
        } else {
            Ok(value)
        }
    }

    pub fn scalar(self, value: f64) -> std::result::Result<f64, String> {
        let value = self.bound(value)?;
        let fits = match self {
            Self::U32 => value.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&value),
            Self::I32 => {
                value.fract() == 0.0 && (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&value)
            }
            Self::U8 => value.fract() == 0.0 && (0.0..=255.0).contains(&value),
            Self::F32 => true,
        };
        if !fits {
            return Err(format!("GPUBuffer value {value} is outside {self:?}"));
        }
        Ok(value)
    }
}

pub fn validate_target(target: &str) -> std::result::Result<(), String> {
    if target
        .strip_prefix("buffer:")
        .is_some_and(|label| !label.is_empty())
    {
        Ok(())
    } else {
        Err("GPUBuffer subject must be buffer:<label>".to_owned())
    }
}

pub fn validate_span(offset: u64, length: Option<u64>) -> std::result::Result<(), String> {
    if !offset.is_multiple_of(4) || length.is_some_and(|length| !length.is_multiple_of(4)) {
        return Err("GPUBuffer offset and length must be multiples of 4 bytes".to_owned());
    }
    if length.is_some_and(|length| offset.checked_add(length).is_none()) {
        return Err("GPUBuffer offset plus length overflows u64".to_owned());
    }
    Ok(())
}

/// One base64 decode, shared by pointer without copying bytes or element arrays.
pub struct Sample {
    pub label: String,
    pub size: u64,
    pub usage: u32,
    pub offset: u64,
    pub submit: u64,
    bytes: Vec<u8>,
}

impl Sample {
    fn decode(
        snapshot: WebGpuBufferSnapshot,
        offset: u64,
        length: Option<u64>,
    ) -> Result<Arc<Self>> {
        if snapshot.offset != offset || offset > snapshot.size {
            bail!(
                "GPUBuffer {:?} returned invalid offset {} for requested {offset} at submit {}",
                snapshot.label,
                snapshot.offset,
                snapshot.submit
            );
        }
        let expected = length.unwrap_or(snapshot.size - offset);
        if !expected.is_multiple_of(4)
            || offset
                .checked_add(expected)
                .is_none_or(|end| end > snapshot.size)
        {
            bail!(
                "GPUBuffer {:?} returned an invalid {expected}-byte range at submit {}",
                snapshot.label,
                snapshot.submit
            );
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&snapshot.data_base64)
            .map_err(|error| {
                eyre!(
                    "GPUBuffer {:?} has invalid base64 at submit {}: {error}",
                    snapshot.label,
                    snapshot.submit
                )
            })?;
        if bytes.len() as u64 != expected {
            bail!(
                "GPUBuffer {:?} returned {} bytes, expected {expected}, at submit {}",
                snapshot.label,
                bytes.len(),
                snapshot.submit
            );
        }
        Ok(Arc::new(Self {
            label: snapshot.label,
            size: snapshot.size,
            usage: snapshot.usage,
            offset,
            submit: snapshot.submit,
            bytes,
        }))
    }

    fn failure(&self, format: Format, index: usize, value: f64, bound: &str) -> String {
        let index = self.offset / format.width() as u64 + index as u64;
        format!(
            "GPUBuffer {:?} element {index} value {value} violates bound {bound} at submit {}",
            self.label, self.submit
        )
    }

    pub fn range(
        &self,
        format: Format,
        min: f64,
        max: f64,
        allow_non_finite: bool,
    ) -> std::result::Result<(), String> {
        let (lower, upper) = (format.bound(min)?, format.bound(max)?);
        let bound = format!("[{min}, {max}]");
        for (index, bytes) in self.bytes.chunks_exact(format.width()).enumerate() {
            let value = format.number(bytes);
            if !value.is_finite() {
                if allow_non_finite {
                    continue;
                }
                return Err(self.failure(format, index, value, &format!("finite {bound}")));
            }
            if value < lower || value > upper {
                return Err(self.failure(format, index, value, &bound));
            }
        }
        Ok(())
    }

    pub fn equals(&self, format: Format, expected: f64) -> std::result::Result<(), String> {
        self.compare(format, expected, false)
    }

    pub fn at_least(&self, format: Format, expected: f64) -> std::result::Result<(), String> {
        self.compare(format, expected, true)
    }

    fn compare(
        &self,
        format: Format,
        expected: f64,
        at_least: bool,
    ) -> std::result::Result<(), String> {
        let required = format.scalar(expected)?;
        let bytes = self.bytes.get(..format.width()).ok_or_else(|| {
            format!(
                "GPUBuffer {:?} has no element at byte offset {} at submit {}",
                self.label, self.offset, self.submit
            )
        })?;
        let value = format.number(bytes);
        let bound = format!(
            "{} {expected}",
            if at_least { "at least" } else { "equals" }
        );
        if !value.is_finite()
            || if at_least {
                value < required
            } else {
                value != required
            }
        {
            return Err(self.failure(format, 0, value, &format!("finite {bound}")));
        }
        Ok(())
    }

    pub fn changes(&self, before: &Self) -> std::result::Result<(), String> {
        if self.label != before.label
            || self.size != before.size
            || self.usage != before.usage
            || self.offset != before.offset
        {
            return Err(format!(
                "GPUBuffer {:?} changed identity from {:?} between submits {} and {}",
                self.label, before.label, before.submit, self.submit
            ));
        }
        if before.submit != 0
            && self.submit != 0
            && before.submit != self.submit
            && before.bytes != self.bytes
        {
            return Ok(());
        }
        let changed = before
            .bytes
            .iter()
            .zip(&self.bytes)
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        Err(format!(
            "GPUBuffer {:?} element {} value {:?} -> {:?} violates bound changed bytes from two different completed submits ({} -> {})",
            self.label,
            self.offset + changed as u64,
            before.bytes.get(changed),
            self.bytes.get(changed),
            before.submit,
            self.submit,
        ))
    }

    pub fn print(&self, format: Format, stride: usize) -> Result<()> {
        if stride == 0 {
            bail!("GPUBuffer stride must be at least 1");
        }
        let count = self.bytes.len() / format.width();
        let stride = stride.min(count.max(1));
        #[derive(serde::Serialize)]
        struct Header<'a> {
            label: &'a str,
            size: u64,
            usage: u32,
            submit: u64,
            offset: u64,
            count: usize,
        }
        #[derive(serde::Serialize)]
        struct Output<'a> {
            buffer: [Header<'a>; 1],
        }
        let header = Output {
            buffer: [Header {
                label: &self.label,
                size: self.size,
                usage: self.usage,
                submit: self.submit,
                offset: self.offset,
                count,
            }],
        };
        println!(
            "{}",
            toon_format::encode_default(&header).map_err(|error| eyre!(error.to_string()))?
        );
        let columns = if stride == 1 {
            "value".to_owned()
        } else {
            (0..stride)
                .map(|index| format!("value{index}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        println!("elements[{}]{{index,{columns}}}:", count.div_ceil(stride));
        for (row, bytes) in self.bytes.chunks(stride * format.width()).enumerate() {
            let index = self.offset / format.width() as u64 + (row * stride) as u64;
            print!("  {index}");
            let mut elements = bytes.chunks_exact(format.width());
            for _ in 0..stride {
                match elements.next() {
                    Some(bytes) => print!(",{}", format.text(bytes)),
                    None => print!(",null"),
                }
            }
            println!();
        }
        Ok(())
    }
}

pub async fn read(
    client: &mut Client,
    target: &str,
    offset: u64,
    length: Option<u64>,
) -> Result<Option<Arc<Sample>>> {
    validate_target(target).map_err(|error| eyre!(error))?;
    validate_span(offset, length).map_err(|error| eyre!(error))?;
    let answer = match client
        .diagnostics(&DiagnosticsRequest::WebGpuBuffer {
            target: target.to_owned(),
            offset,
            length,
        })
        .await
    {
        Err(error) if is_pending(&error) => return Ok(None),
        answer => answer?,
    };
    match answer.response {
        DebugResponse::WebGpuBuffer(snapshot) => Sample::decode(snapshot, offset, length).map(Some),
        other => bail!("asked for GPUBuffer {target:?}, got {other:?}"),
    }
}
