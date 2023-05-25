use std::{collections::BTreeMap, marker::PhantomData};

use ff::PrimeFieldBits;
use halo2_proofs::{
    circuit::{AssignedCell, Layouter, SimpleFloorPlanner},
    plonk::{Advice, Circuit, Column, ConstraintSystem, Error, Instance},
};
use ndarray::{array, Array1, Array2, Array3};

use crate::gadgets::{
    bits2num::{Bits2NumChip, Bits2NumChipConfig, Bits2NumInstruction},
    bloom_filter::{BloomFilterChip, BloomFilterChipConfig},
    bloom_filter::{BloomFilterConfig, BloomFilterInstructions},
    hash::{HashChip, HashConfig, HashInstructions},
    range_check::RangeCheckConfig,
    response_accumulator::ResponseAccumulatorInstructions,
};
use crate::gadgets::{
    hash::HashFunctionConfig,
    response_accumulator::{ResponseAccumulatorChip, ResponseAccumulatorChipConfig},
};

use super::greater_than::{GreaterThanChip, GreaterThanChipConfig, GreaterThanInstructions};

pub trait WnnInstructions<F: PrimeFieldBits> {
    /// Given an input vector, predicts the score for each class.
    fn predict(
        &self,
        layouter: impl Layouter<F>,
        image: &Array2<u8>,
    ) -> Result<Vec<AssignedCell<F, F>>, Error>;
}

#[derive(Debug, Clone)]
struct WnnConfig {
    hash_function_config: HashFunctionConfig,
    bloom_filter_config: BloomFilterConfig,
}

#[derive(Clone, Debug)]
pub struct WnnChipConfig<F: PrimeFieldBits> {
    greater_than_chip_config: GreaterThanChipConfig,
    hash_chip_config: HashConfig<F>,
    bloom_filter_chip_config: BloomFilterChipConfig,
    response_accumulator_chip_config: ResponseAccumulatorChipConfig,
    bit2num_chip_config: Bits2NumChipConfig,
    input: Column<Advice>,
}

/// Implements a BTHOWeN- style weightless neural network.
///
/// This happens in three steps:
/// 1. The [`HashChip`] is used to range-check and hash the inputs.
/// 2. The [`BloomFilterChip`] is used to look up the bloom filter responses
///    (for each input and each class).
/// 3. The [`ResponseAccumulatorChip`] is used to accumulate the responses.
struct WnnChip<F: PrimeFieldBits> {
    greater_than_chip: GreaterThanChip<F>,
    bits2num_chip: Bits2NumChip<F>,
    hash_chip: HashChip<F>,
    bloom_filter_chip: BloomFilterChip<F>,
    response_accumulator_chip: ResponseAccumulatorChip<F>,

    binarization_thresholds: Array3<u16>,
    input_permutation: Array1<u64>,

    config: WnnChipConfig<F>,

    n_classes: usize,
    n_inputs: usize,
}

impl<F: PrimeFieldBits> WnnChip<F> {
    fn construct(
        config: WnnChipConfig<F>,
        bloom_filter_arrays: Array3<bool>,
        binarization_thresholds: Array3<u16>,
        input_permutation: Array1<u64>,
    ) -> Self {
        let shape = bloom_filter_arrays.shape();
        let n_classes = shape[0];
        let n_inputs = shape[1];
        let n_filters = shape[2];

        // Flatten array: from shape (C, N, B) to (C * N, B)
        let bloom_filter_arrays_flat = bloom_filter_arrays
            .into_shape((n_classes * n_inputs, n_filters))
            .unwrap();

        let greater_than_chip = GreaterThanChip::construct(config.greater_than_chip_config.clone());
        let bits2num_chip = Bits2NumChip::construct(config.bit2num_chip_config.clone());
        let hash_chip = HashChip::construct(config.hash_chip_config.clone());
        let bloom_filter_chip = BloomFilterChip::construct(
            config.bloom_filter_chip_config.clone(),
            &bloom_filter_arrays_flat,
        );
        let response_accumulator_chip =
            ResponseAccumulatorChip::construct(config.response_accumulator_chip_config.clone());

        WnnChip {
            greater_than_chip,
            bits2num_chip,
            hash_chip,
            bloom_filter_chip,
            response_accumulator_chip,

            binarization_thresholds,
            input_permutation,

            config,

            n_classes,
            n_inputs,
        }
    }

    fn configure(
        meta: &mut ConstraintSystem<F>,
        advice_columns: [Column<Advice>; 6],
        wnn_config: WnnConfig,
    ) -> WnnChipConfig<F> {
        let bloom_filter_chip_config = BloomFilterChip::configure(
            meta,
            advice_columns,
            wnn_config.bloom_filter_config.clone(),
        );
        let greater_than_chip_config = GreaterThanChip::configure(
            meta,
            advice_columns[0],
            advice_columns[1],
            advice_columns[2],
            advice_columns[3],
            // Re-use byte column of the bloom filter
            bloom_filter_chip_config.byte_column,
        );
        let lookup_range_check_config = RangeCheckConfig::configure(
            meta,
            advice_columns[0],
            // Re-use byte column of the bloom filter
            bloom_filter_chip_config.byte_column,
        );
        let hash_chip_config = HashChip::configure(
            meta,
            advice_columns[0],
            advice_columns[1],
            advice_columns[2],
            advice_columns[3],
            advice_columns[4],
            lookup_range_check_config,
            wnn_config.hash_function_config.clone(),
        );
        let response_accumulator_chip_config =
            ResponseAccumulatorChip::configure(meta, advice_columns[0..5].try_into().unwrap());

        let bit2num_chip_config =
            Bits2NumChip::configure(meta, advice_columns[1], advice_columns[5]);

        WnnChipConfig {
            greater_than_chip_config,
            hash_chip_config,
            bloom_filter_chip_config,
            response_accumulator_chip_config,
            bit2num_chip_config,
            input: advice_columns[0],
        }
    }

    pub fn load(&mut self, layouter: &mut impl Layouter<F>) -> Result<(), Error> {
        self.bloom_filter_chip.load(layouter)
    }
}

impl<F: PrimeFieldBits> WnnInstructions<F> for WnnChip<F> {
    fn predict(
        &self,
        mut layouter: impl Layouter<F>,
        image: &Array2<u8>,
    ) -> Result<Vec<AssignedCell<F, F>>, Error> {
        let (width, height) = (image.shape()[0], image.shape()[1]);

        let mut intensity_cells: BTreeMap<(usize, usize), AssignedCell<F, F>> = BTreeMap::new();
        let mut bit_cells = vec![];

        for b in 0..self.binarization_thresholds.shape()[2] {
            for i in 0..width {
                for j in 0..height {
                    let threshold = self.binarization_thresholds[(i, j, b)];
                    assert!(threshold <= 256);

                    let bit_cell = if threshold == 0 {
                        // If the threshold is zero, the bit is always one, regardless of the of the intensity.
                        layouter.assign_region(
                            || "bit is one",
                            |mut region| {
                                region.assign_advice_from_constant(
                                    || "gt",
                                    self.config.input,
                                    0,
                                    F::ONE,
                                )
                            },
                        )?
                    } else {
                        // The result should be true if the intensity is greater or equal than the threshold,
                        // but the gadget only implements greater than, so we need to subtract 1 from the threshold.
                        // Because we already handled the threshold == 0 case, this means that `t` is now in the
                        // range [0, 255], which is required by the greater than gadget.
                        let t = F::from((self.binarization_thresholds[(i, j, b)] - 1) as u64);

                        match intensity_cells.get(&(i, j)) {
                            None => {
                                // For the first cell, we want to remember the intensity cell, so that we can
                                // add a copy constraint for the other thresholds.
                                let (intensity_cell, bit_cell) =
                                    self.greater_than_chip.greater_than_witness(
                                        &mut layouter,
                                        F::from(image[(i, j)] as u64),
                                        t,
                                    )?;
                                intensity_cells.insert((i, j), intensity_cell);
                                bit_cell
                            }
                            Some(first_cell) => {
                                // For the other cells, we want to add a copy constraint to the first cell.
                                self.greater_than_chip.greater_than_copy(
                                    &mut layouter,
                                    first_cell,
                                    t,
                                )?
                            }
                        }
                    };
                    bit_cells.push(bit_cell);
                }
            }
        }

        // Permute input bits
        let permuted_inputs = self
            .input_permutation
            .iter()
            .map(|i| bit_cells[*i as usize].clone())
            .collect::<Vec<_>>();

        let num_bit_size = self.config.hash_chip_config.hash_function_config.n_bits;

        // Convert the input bits to a group of field element that can be hashed
        let joint_inputs = permuted_inputs
            .chunks_exact(num_bit_size)
            .map(|chunk| {
                self.bits2num_chip
                    .convert_le(&mut layouter, Vec::from(chunk))
            })
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(self.n_inputs, joint_inputs.len());

        let hashes = joint_inputs
            .into_iter()
            .map(|hash_input| {
                self.hash_chip
                    .hash(layouter.namespace(|| "hash"), hash_input)
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut responses = vec![];
        for c in 0..self.n_classes {
            responses.push(Vec::new());
            for (i, hash) in hashes.clone().into_iter().enumerate() {
                let array_index = c * hashes.len() + i;
                responses[c].push(self.bloom_filter_chip.bloom_lookup(
                    &mut layouter,
                    hash,
                    F::from(array_index as u64),
                )?);
            }
        }

        responses
            .iter()
            .map(|class_responses| {
                self.response_accumulator_chip
                    .accumulate_responses(&mut layouter, class_responses)
            })
            .collect::<Result<Vec<_>, _>>()
    }
}

#[derive(Debug, Clone)]
pub struct WnnCircuitConfig<F: PrimeFieldBits> {
    wnn_chip_config: WnnChipConfig<F>,
    instance_column: Column<Instance>,
}

#[derive(Clone)]
pub struct WnnCircuitParams {
    pub p: u64,
    pub l: usize,
    pub n_hashes: usize,
    pub bits_per_hash: usize,
    pub bits_per_filter: usize,
}

pub struct WnnCircuit<F: PrimeFieldBits> {
    image: Array2<u8>,
    bloom_filter_arrays: Array3<bool>,
    binarization_thresholds: Array3<u16>,
    input_permutation: Array1<u64>,
    params: WnnCircuitParams,
    _marker: PhantomData<F>,
}

impl<F: PrimeFieldBits> WnnCircuit<F> {
    pub fn new(
        image: Array2<u8>,
        bloom_filter_arrays: Array3<bool>,
        binarization_thresholds: Array3<u16>,
        input_permutation: Array1<u64>,
        params: WnnCircuitParams,
    ) -> Self {
        Self {
            image,
            bloom_filter_arrays,
            binarization_thresholds,
            input_permutation,
            params,
            _marker: PhantomData,
        }
    }

    pub fn plot(&self, filename: &str, k: u32) {
        use plotters::prelude::*;

        let root = BitMapBackend::new(filename, (1024, 1 << (k + 3))).into_drawing_area();
        root.fill(&WHITE).unwrap();
        let root = root.titled("WNN Layout", ("sans-serif", 60)).unwrap();
        halo2_proofs::dev::CircuitLayout::default()
            .show_labels(true)
            .render(k, self, &root)
            .unwrap();
    }
}

impl Default for WnnCircuitParams {
    fn default() -> Self {
        unimplemented!("Parameters have to be specified manually!")
    }
}

impl<F: PrimeFieldBits> Circuit<F> for WnnCircuit<F> {
    type Config = WnnCircuitConfig<F>;
    type FloorPlanner = SimpleFloorPlanner;
    type Params = WnnCircuitParams;

    fn without_witnesses(&self) -> Self {
        Self {
            image: array![[]],
            bloom_filter_arrays: array![[[]]],
            binarization_thresholds: array![[[]]],
            input_permutation: array![],
            params: self.params.clone(),
            _marker: PhantomData,
        }
    }

    fn params(&self) -> Self::Params {
        self.params.clone()
    }

    fn configure_with_params(meta: &mut ConstraintSystem<F>, params: Self::Params) -> Self::Config {
        let instance_column = meta.instance_column();

        let advice_columns = [
            meta.advice_column(),
            meta.advice_column(),
            meta.advice_column(),
            meta.advice_column(),
            meta.advice_column(),
            meta.advice_column(),
        ];

        for advice in advice_columns {
            meta.enable_equality(advice);
        }
        meta.enable_equality(instance_column);

        let constants = meta.fixed_column();
        meta.enable_constant(constants);

        let bloom_filter_config = BloomFilterConfig {
            n_hashes: params.n_hashes,
            bits_per_hash: params.bits_per_hash,
        };
        let hash_function_config = HashFunctionConfig {
            p: params.p,
            l: params.l,
            n_bits: params.bits_per_filter,
        };
        let wnn_config = WnnConfig {
            bloom_filter_config,
            hash_function_config,
        };
        WnnCircuitConfig {
            wnn_chip_config: WnnChip::configure(meta, advice_columns, wnn_config),
            instance_column,
        }
    }

    fn synthesize(
        &self,
        config: Self::Config,
        mut layouter: impl Layouter<F>,
    ) -> Result<(), Error> {
        let mut wnn_chip = WnnChip::construct(
            config.wnn_chip_config,
            self.bloom_filter_arrays.clone(),
            self.binarization_thresholds.clone(),
            self.input_permutation.clone(),
        );
        wnn_chip.load(&mut layouter)?;

        let result = wnn_chip.predict(layouter.namespace(|| "wnn"), &self.image)?;

        for i in 0..result.len() {
            layouter.constrain_instance(result[i].cell(), config.instance_column, i)?;
        }

        Ok(())
    }

    fn configure(_meta: &mut ConstraintSystem<F>) -> Self::Config {
        unimplemented!("configure_with_params should be used!")
    }
}

// #[cfg(test)]
// mod tests {

//     use halo2_proofs::dev::MockProver;
//     use halo2_proofs::halo2curves::bn256::Fr as Fp;
//     use ndarray::Array3;

//     use super::{WnnCircuit, WnnCircuitParams};

//     const PARAMS: WnnCircuitParams = WnnCircuitParams {
//         p: (1 << 21) - 9,
//         l: 20,
//         n_hashes: 2,
//         bits_per_hash: 10,
//         bits_per_filter: 15,
//     };

//     #[test]
//     fn test() {
//         let k = 13;
//         let input = vec![
//             true, false, true, false, false, false, true, false, false, false, false, true, false,
//             false, false, true, false, false, false, false, true, true, true, true, false, true,
//             false, true, true, true,
//         ];
//         // First, we join the bits into two 15 bit numbers 2117 and 30177
//         // Then joint input numbers hash to the following indices:
//         // - 2117 -> (2117^3) % (2^21 - 9) % (1024^2) = 260681
//         //   - 260681 % 1024 = 585
//         //   - 260681 // 1024 = 254
//         // - 30177 -> (30177^3) % (2^21 - 9) % (1024^2) = 260392
//         //   - 260392 % 1024 = 296
//         //   - 260392 // 1024 = 254
//         // We'll set the bloom filter such that we get one positive response for he first
//         // class and two positive responses for the second class.
//         let mut bloom_filter_arrays = Array3::<u8>::ones((3, 2, 1024)).mapv(|_| false);
//         // First class
//         bloom_filter_arrays[[0, 0, 585]] = true;
//         bloom_filter_arrays[[0, 0, 254]] = true;
//         bloom_filter_arrays[[0, 1, 296]] = true;
//         // Second class
//         bloom_filter_arrays[[1, 0, 585]] = true;
//         bloom_filter_arrays[[1, 0, 254]] = true;
//         bloom_filter_arrays[[1, 1, 296]] = true;
//         bloom_filter_arrays[[1, 1, 254]] = true;

//         let circuit = WnnCircuit::<Fp>::new(input, bloom_filter_arrays, PARAMS);

//         let expected_result = vec![Fp::from(1), Fp::from(2)];

//         let prover = MockProver::run(k, &circuit, vec![expected_result]).unwrap();
//         prover.assert_satisfied();
//     }

//     #[test]
//     fn plot() {
//         let bloom_filter_arrays = Array3::<u8>::ones((2, 2, 1024)).mapv(|_| true);
//         // This is the input that will be joint into [2, 7]
//         let inputs = vec![
//             false, true, false, false, false, false, false, false, false, false, false, false,
//             false, false, false, true, true, true, false, false, false, false, false, false, false,
//             false, false, false, false, false,
//         ];
//         WnnCircuit::<Fp>::new(inputs, bloom_filter_arrays, PARAMS).plot("wnn-layout.png", 9);
//     }
// }
