use ff::{Field, PrimeField};
use halo2_proofs::circuit::{AssignedCell, Layouter, Value};
use halo2_proofs::plonk::{Advice, Column, ConstraintSystem, Constraints, Error, Selector};
use halo2_proofs::poly::Rotation;
use std::marker::PhantomData;

pub(crate) trait Bits2NumInstruction<F: Field> {
    /// Convert the bits in little endian order to a number
    fn convert_be(
        &self,
        layouter: &mut impl Layouter<F>,
        bits: Vec<AssignedCell<F, F>>,
    ) -> Result<AssignedCell<F, F>, Error>;

    /// Convert the bits in big endian order to a number
    fn convert_le(
        &self,
        layouter: &mut impl Layouter<F>,
        bits: Vec<AssignedCell<F, F>>,
    ) -> Result<AssignedCell<F, F>, Error>;
}

#[derive(Debug, Clone)]
pub(crate) struct Bits2NumConfig {
    /// How many bits are to be converted into a single number
    /// Max values is log(|F|)
    pub(crate) num_bit_size: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct Bits2NumChipConfig {
    pub(crate) selector: Selector,
    pub(crate) input: Column<Advice>,
    pub(crate) output: Column<Advice>,
    pub(crate) bit2num_config: Bits2NumConfig,
}

pub(crate) struct Bits2NumChip<F: Field> {
    config: Bits2NumChipConfig,
    _marker: PhantomData<F>,
}

impl<F: PrimeField> Bits2NumChip<F> {
    pub(crate) fn construct(config: Bits2NumChipConfig) -> Self {
        Bits2NumChip {
            config,
            _marker: PhantomData::default(),
        }
    }

    pub(crate) fn configure(
        meta: &mut ConstraintSystem<F>,
        input: Column<Advice>,
        output: Column<Advice>,
        bit2num_config: Bits2NumConfig,
    ) -> Bits2NumChipConfig {
        let selector = meta.selector();

        meta.create_gate("next_num_constraint", |cs| {
            let bit_val = cs.query_advice(input, Rotation::cur());
            let prev_num_val = cs.query_advice(output, Rotation::cur());
            let num_val = cs.query_advice(output, Rotation::next());

            let selector = cs.query_selector(selector);

            Constraints::with_selector(
                selector,
                vec![num_val - (prev_num_val * F::from(2) + bit_val)],
            )
        });

        Bits2NumChipConfig {
            selector,
            input,
            output,
            bit2num_config,
        }
    }
}

impl<F: PrimeField> Bits2NumInstruction<F> for Bits2NumChip<F> {
    fn convert_be(
        &self,
        layouter: &mut impl Layouter<F>,
        bits: Vec<AssignedCell<F, F>>,
    ) -> Result<AssignedCell<F, F>, Error> {
        let res = layouter.assign_region(
            || "bits2num",
            |mut region| {
                assert_eq!(
                    bits.len(),
                    self.config.bit2num_config.num_bit_size,
                    "Incorrect amount of bits provided!"
                );
                assert!(
                    self.config.bit2num_config.num_bit_size <= F::CAPACITY as usize,
                    "Number of bits is too large!"
                );

                let mut num_val = Value::known(F::from(0));

                let mut num_val_cell = region
                    .assign_advice_from_constant(
                        || format!("prev_num_val {}", 0),
                        self.config.output,
                        0,
                        F::ZERO,
                    )?;

                for i in 0..self.config.bit2num_config.num_bit_size {
                    self.config.selector.enable(&mut region, i).unwrap();

                    num_val = num_val * Value::known(F::from(2)) + bits[i].value();

                    num_val_cell = region
                        .assign_advice(
                            || format!("num_val {}", i + 1),
                            self.config.output,
                            i + 1,
                            || num_val,
                        )?;

                    bits[i]
                        .copy_advice(
                            || format!("input bit {}", i),
                            &mut region,
                            self.config.input,
                            i,
                        )?;
                }

                Ok(num_val_cell)
            },
        );

        res
    }

    fn convert_le(
        &self,
        layouter: &mut impl Layouter<F>,
        mut bits: Vec<AssignedCell<F, F>>,
    ) -> Result<AssignedCell<F, F>, Error> {
        // Reverse bits to convert from litlle to big endian
        bits.reverse();

        self.convert_be(layouter, bits)
    }
}

#[cfg(test)]
mod test {
    use crate::gadgets::bits2num::{
        Bits2NumChip, Bits2NumChipConfig, Bits2NumConfig, Bits2NumInstruction,
    };
    use ff::PrimeField;
    use halo2_proofs::circuit::{Layouter, SimpleFloorPlanner, Value};
    use halo2_proofs::dev::MockProver;
    use halo2_proofs::halo2curves::bn256::{Fr as Fp, Fr};
    use halo2_proofs::plonk::{Circuit, Column, ConstraintSystem, Error, Instance};

    struct Bits2NumTestCircuit {
        input: Vec<bool>,
        params: usize,
        mode: BiteMode,
    }

    enum BiteMode {
        BE,
        LE,
    }

    #[derive(Clone)]
    struct Bits2NumCircuitConfig {
        bits2num_chip_conf: Bits2NumChipConfig,
        pub_input: Column<Instance>,
    }

    impl<F: PrimeField> Circuit<F> for Bits2NumTestCircuit {
        type Config = Bits2NumCircuitConfig;
        type FloorPlanner = SimpleFloorPlanner;
        type Params = usize;

        fn without_witnesses(&self) -> Self {
            Self {
                input: vec![],
                params: self.params,
                mode: BiteMode::LE,
            }
        }

        fn params(&self) -> Self::Params {
            self.params
        }

        fn configure(_meta: &mut ConstraintSystem<F>) -> Self::Config {
            unimplemented!("configure_with_params should be used!")
        }

        fn configure_with_params(
            meta: &mut ConstraintSystem<F>,
            _params: Self::Params,
        ) -> Self::Config {
            let input = meta.advice_column();
            let output = meta.advice_column();
            let constants = meta.fixed_column();
            let bit2num_config = Bits2NumConfig {
                num_bit_size: _params,
            };
            let pub_input = meta.instance_column();

            meta.enable_equality(pub_input);
            meta.enable_equality(output);
            meta.enable_equality(input);
            meta.enable_constant(constants);

            Bits2NumCircuitConfig {
                bits2num_chip_conf: Bits2NumChip::configure(meta, input, output, bit2num_config),
                pub_input,
            }
        }

        fn synthesize(
            &self,
            config: Self::Config,
            mut layouter: impl Layouter<F>,
        ) -> Result<(), Error> {
            let mut assigned_input = vec![];

            for (i, bit) in self.input.iter().enumerate() {
                let bit_val = F::from(*bit as u64);
                let bit_cell = layouter.assign_region(
                    || format!("input bit {}", i),
                    |mut region| {
                        let bit_cell = region.assign_advice(
                            || "bit",
                            config.bits2num_chip_conf.input,
                            i,
                            || Value::known(bit_val),
                        )?;

                        Ok(bit_cell)
                    },
                )?;

                assigned_input.push(bit_cell);
            }

            let bit2num = Bits2NumChip::<F>::construct(config.bits2num_chip_conf);

            let res = match self.mode {
                BiteMode::BE => bit2num.convert_be(&mut layouter, assigned_input)?,
                BiteMode::LE => bit2num.convert_le(&mut layouter, assigned_input)?,
            };

            layouter.constrain_instance(res.cell(), config.pub_input.clone(), 0)?;

            Ok(())
        }
    }

    #[test]
    fn test_bits2num_be_chip() {
        let params = 4;
        let input = vec![true, false, true, false];

        let circuit = Bits2NumTestCircuit {
            input: input.clone(),
            params,
            mode: BiteMode::BE,
        };

        let answer = 10;

        let prover = MockProver::<Fp>::run(5, &circuit, vec![vec![Fr::from(answer)]]).unwrap();

        prover.assert_satisfied()
    }

    #[test]
    fn test_bits2num_le_chip() {
        let params = 4;
        let input = vec![true, false, true, false];

        let circuit = Bits2NumTestCircuit {
            input: input.clone(),
            params,
            mode: BiteMode::LE,
        };

        let answer = 5;

        let prover = MockProver::<Fp>::run(5, &circuit, vec![vec![Fr::from(answer)]]).unwrap();

        prover.assert_satisfied()
    }
}
