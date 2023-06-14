//! Utilities for loading images and WNNs from disk.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use halo2_proofs::halo2curves::bn256::{Bn256, Fr, G1Affine};
use halo2_proofs::plonk::{ProvingKey, VerifyingKey};
use halo2_proofs::poly::commitment::Params;
use halo2_proofs::poly::kzg::commitment::ParamsKZG;
use halo2_proofs::SerdeFormat::RawBytes;
use hdf5::{File as Hdf5File, Result as Hdf5Result};
use image::ImageError;
use ndarray::{s, Array, Array2, Array3};
use ndarray::{Ix1, Ix3};
use serde::{Deserialize, Serialize};

use crate::gadgets::wnn::WnnCircuitParams;
use crate::gadgets::WnnCircuit;
use crate::wnn::Wnn;

/// Loads a grayscale image from disk, returning the first channel.
pub fn load_grayscale_image(img_path: &Path) -> Result<Array2<u8>, ImageError> {
    let image = image::open(img_path)?.to_rgb8();
    let array: Array3<u8> = Array::from_shape_vec(
        (image.height() as usize, image.width() as usize, 3),
        image.into_raw(),
    )
    .expect("Error converting image to ndarray");

    Ok(array.slice_move(s![.., .., 0]))
}

/// Loads a [`Wnn`] from disk, from a file following [this format](https://github.com/zkp-gravity/BTHOWeN-0g/blob/master/output_format_spec.md).
pub fn load_wnn(path: &Path) -> Hdf5Result<Wnn> {
    let file = Hdf5File::open(path)?;

    let num_classes = file.attr("num_classes")?.read_scalar::<i64>()? as usize;
    let num_inputs = file.attr("num_inputs")?.read_scalar::<i64>()? as usize;
    let bits_per_input = file.attr("bits_per_input")?.read_scalar::<i64>()? as usize;
    let num_filter_inputs = file.attr("num_filter_inputs")?.read_scalar::<i64>()? as usize;
    let num_filter_entries = file.attr("num_filter_entries")?.read_scalar::<i64>()? as usize;
    let num_filter_hashes = file.attr("num_filter_hashes")?.read_scalar::<i64>()? as usize;
    let p = file.attr("p")?.read_scalar::<i64>()? as u64;

    let expected_shape = [
        num_classes,
        num_inputs * bits_per_input / num_filter_inputs,
        num_filter_entries,
    ];
    let bloom_filters = file.dataset("bloom_filters")?;
    let bloom_filters = bloom_filters.read::<bool, Ix3>()?;
    assert_eq!(bloom_filters.shape(), expected_shape);

    let width = (num_inputs as f32).sqrt() as usize;
    let expected_shape = [width, width, bits_per_input];
    let binarization_thresholds = file.dataset("binarization_thresholds")?;
    let binarization_thresholds = binarization_thresholds.read::<f32, Ix3>()?;
    assert_eq!(binarization_thresholds.shape(), expected_shape);

    // Quantize binarization thresholds.
    // This should make no difference to the accuracy of the model,
    // because images are quantized to u8 anyway.
    // Note that:
    // - We use ceil(), because <u8> >= <f32> <==> <u8> >= <f32>.ceil() as u8
    // - We clamp at 0, because intensities cannot be negative
    // - We clamp at **256**, because intensities cannot be greater than 255
    //   Note that thresholds set to 256 will never be reached!
    //   Also note that for this reason, we can't use u8 to store the thresholds.
    let binarization_thresholds = binarization_thresholds * 255.0;
    let binarization_thresholds =
        binarization_thresholds.map(|x| x.ceil().max(0.0).min(256.0) as u16);

    let input_order = file.dataset("input_order")?;
    let input_order = input_order.read::<u64, Ix1>()?;
    let num_input_bits = num_inputs * bits_per_input;
    assert_eq!(input_order.shape(), [num_input_bits]);

    Ok(Wnn::new(
        num_classes,
        num_filter_entries,
        num_filter_hashes,
        num_filter_inputs,
        p,
        bloom_filters,
        input_order,
        binarization_thresholds,
    ))
}

/// Given a path like `data/MNIST/png/0000_7.png`, read the correct class (in this case 7).
pub fn parse_png_file(img_path: &Path) -> Option<usize> {
    match img_path.extension() {
        Some(extension) => {
            if extension == "png" {
                Some(
                    img_path
                        .file_stem()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .chars()
                        .last()
                        .unwrap()
                        .to_digit(10)
                        .unwrap()
                        .try_into()
                        .unwrap(),
                )
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn write_srs(srs: &ParamsKZG<Bn256>, path: &Path) {
    let f = File::create(path).expect("Unable to create file");
    let mut writer = BufWriter::new(f);
    srs.write(&mut writer).expect("Unable to write to file");
    writer.flush().expect("Unable to flush file");
}

pub fn read_srs(path: &Path) -> ParamsKZG<Bn256> {
    let f = File::open(path).expect("Unable to open file");
    let mut reader = BufReader::new(f);
    ParamsKZG::read(&mut reader).expect("Unable to read from file")
}

pub fn write_keys(pk: &ProvingKey<G1Affine>, pk_path: &Path, vk_path: &Path) {
    let f = File::create(pk_path).expect("Unable to create file");
    let mut writer = BufWriter::new(f);
    pk.write(&mut writer, RawBytes)
        .expect("Unable to write to file");
    writer.flush().expect("Unable to flush file");

    let f = File::create(vk_path).expect("Unable to create file");
    let mut writer = BufWriter::new(f);
    pk.get_vk()
        .write(&mut writer, halo2_proofs::SerdeFormat::RawBytes)
        .expect("Unable to write to file");
    writer.flush().expect("Unable to flush file");
}

pub fn write_circuit_params(circuit_params: &WnnCircuitParams, path: &Path) {
    let encoded = serde_json::to_string(circuit_params).expect("Error serializing circuit params");
    let mut file = File::create(path).expect("Unable to create file");
    file.write_all(encoded.as_bytes())
        .expect("Unable to write to file");
}

pub fn read_circuit_params(path: &Path) -> WnnCircuitParams {
    let mut file = File::open(path).expect("Unable to open file");
    let mut data = String::new();
    file.read_to_string(&mut data).expect("Unable to read file");
    serde_json::from_str(&data).expect("Error deserializing circuit params")
}

pub fn read_pk(path: &Path, circuit_params: WnnCircuitParams) -> ProvingKey<G1Affine> {
    let f = File::open(path).expect("Unable to open file");
    let mut reader = BufReader::new(f);
    ProvingKey::read::<_, WnnCircuit<_>>(&mut reader, RawBytes, circuit_params)
        .expect("Unable to read from file")
}

pub fn read_vk(path: &Path, circuit_params: WnnCircuitParams) -> VerifyingKey<G1Affine> {
    let f = File::open(path).expect("Unable to open file");
    let mut reader = BufReader::new(f);
    VerifyingKey::read::<_, WnnCircuit<_>>(&mut reader, RawBytes, circuit_params)
        .expect("Unable to read from file")
}

#[derive(Serialize, Deserialize)]
pub struct ProofWithOutput {
    pub proof: Vec<u8>,
    pub output: Vec<Fr>,
}

impl From<(Vec<u8>, Vec<Fr>)> for ProofWithOutput {
    fn from((proof, output): (Vec<u8>, Vec<Fr>)) -> Self {
        Self { proof, output }
    }
}

impl From<ProofWithOutput> for (Vec<u8>, Vec<Fr>) {
    fn from(proof_with_output: ProofWithOutput) -> Self {
        (proof_with_output.proof, proof_with_output.output)
    }
}

impl ProofWithOutput {
    pub fn write(&self, path: &Path) {
        let encoded = serde_json::to_string(self).expect("Error serializing proof with output");
        let mut file = File::create(path).expect("Unable to create file");
        file.write_all(encoded.as_bytes())
            .expect("Unable to write to file");
    }

    pub fn read(path: &Path) -> Self {
        let mut file = File::open(path).expect("Unable to open file");
        let mut data = String::new();
        file.read_to_string(&mut data).expect("Unable to read file");
        serde_json::from_str(&data).expect("Error deserializing proof with output")
    }
}
