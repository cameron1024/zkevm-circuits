use crate::{
    evm_circuit::util::{
        and, constraint_builder::BaseConstraintBuilder, not, or, select, RandomLinearCombination,
    },
    table::{BytecodeFieldTag, BytecodeTable, DynamicTableColumns, KeccakTable},
    util::{Challenges, Expr, SubCircuit, SubCircuitConfig},
    witness,
};
use bus_mapping::evm::OpcodeId;
use eth_types::{Field, ToLittleEndian, Word};
use gadgets::is_zero::{IsZeroChip, IsZeroConfig, IsZeroInstruction};
use halo2_proofs::{
    circuit::{Layouter, Region, Value},
    plonk::{
        Advice, Column, ConstraintSystem, Error, Expression, Fixed, SecondPhase, Selector,
        VirtualCells,
    },
    poly::Rotation,
};
use keccak256::plain::Keccak;
use std::vec;

use super::param::PUSH_TABLE_WIDTH;
/// Public data for the bytecode
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BytecodeRow<F: Field> {
    code_hash: Word,
    tag: F,
    index: F,
    is_code: F,
    value: F,
}

/// Unrolled bytecode
#[derive(Clone, Debug, PartialEq)]
pub struct UnrolledBytecode<F: Field> {
    pub(crate) bytes: Vec<u8>,
    rows: Vec<BytecodeRow<F>>,
}

#[derive(Clone, Debug)]
/// Bytecode circuit configuration
pub struct BytecodeCircuitConfig<F> {
    minimum_rows: usize,
    q_enable: Column<Fixed>,
    q_first: Column<Fixed>,
    q_last: Column<Fixed>,
    bytecode_table: BytecodeTable,
    push_data_left: Column<Advice>,
    value_rlc: Column<Advice>,
    length: Column<Advice>,
    push_data_size: Column<Advice>,
    push_data_left_inv: Column<Advice>,
    push_data_left_is_zero: IsZeroConfig<F>,
    push_table: [Column<Fixed>; PUSH_TABLE_WIDTH],
    // External tables
    pub(crate) keccak_table: KeccakTable,
}

/// Circuit configuration arguments
pub struct BytecodeCircuitConfigArgs<F: Field> {
    /// BytecodeTable
    pub bytecode_table: BytecodeTable,
    /// KeccakTable
    pub keccak_table: KeccakTable,
    /// Challenges
    pub challenges: Challenges<Expression<F>>,
}

impl<F: Field> SubCircuitConfig<F> for BytecodeCircuitConfig<F> {
    type ConfigArgs = BytecodeCircuitConfigArgs<F>;

    /// Return a new BytecodeCircuitConfig
    fn new(
        meta: &mut ConstraintSystem<F>,
        Self::ConfigArgs {
            bytecode_table,
            keccak_table,
            challenges,
        }: Self::ConfigArgs,
    ) -> Self {
        let q_enable = meta.fixed_column();
        let q_first = meta.fixed_column();
        let q_last = meta.fixed_column();
        let value = bytecode_table.value;
        let push_data_left = meta.advice_column();
        let value_rlc = meta.advice_column_in(SecondPhase);
        let length = meta.advice_column();
        let push_data_size = meta.advice_column();
        let push_data_left_inv = meta.advice_column();
        let push_table = array_init::array_init(|_| meta.fixed_column());

        let is_header_to_header = |meta: &mut VirtualCells<F>| {
            and::expr(vec![
                not::expr(meta.query_advice(bytecode_table.tag, Rotation::cur())),
                not::expr(meta.query_advice(bytecode_table.tag, Rotation::next())),
            ])
        };

        let is_header_to_byte = |meta: &mut VirtualCells<F>| {
            and::expr(vec![
                not::expr(meta.query_advice(bytecode_table.tag, Rotation::cur())),
                meta.query_advice(bytecode_table.tag, Rotation::next()),
            ])
        };

        let is_byte_to_header = |meta: &mut VirtualCells<F>| {
            and::expr(vec![
                meta.query_advice(bytecode_table.tag, Rotation::cur()),
                not::expr(meta.query_advice(bytecode_table.tag, Rotation::next())),
            ])
        };

        let is_byte_to_byte = |meta: &mut VirtualCells<F>| {
            and::expr(vec![
                meta.query_advice(bytecode_table.tag, Rotation::cur()),
                meta.query_advice(bytecode_table.tag, Rotation::next()),
            ])
        };

        let is_header = |meta: &mut VirtualCells<F>| {
            not::expr(meta.query_advice(bytecode_table.tag, Rotation::cur()))
        };

        let is_byte =
            |meta: &mut VirtualCells<F>| meta.query_advice(bytecode_table.tag, Rotation::cur());

        // A byte is an opcode when `push_data_left == 0` on the current row,
        // else it's push data.
        let push_data_left_is_zero = IsZeroChip::configure(
            meta,
            |meta| meta.query_fixed(q_enable, Rotation::cur()),
            |meta| meta.query_advice(push_data_left, Rotation::cur()),
            push_data_left_inv,
        );

        // When q_first || q_last ->
        // assert cur.tag == Header
        meta.create_gate("first and last row", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_zero(
                "cur.tag == Header",
                meta.query_advice(bytecode_table.tag, Rotation::cur()),
            );

            cb.gate(and::expr(vec![
                meta.query_fixed(q_enable, Rotation::cur()),
                or::expr(vec![
                    meta.query_fixed(q_first, Rotation::cur()),
                    meta.query_fixed(q_last, Rotation::cur()),
                ]),
            ]))
        });

        // When is_header ->
        // assert cur.index == 0
        // assert cur.value == cur.length
        meta.create_gate("Header row", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_zero(
                "cur.index == 0",
                meta.query_advice(bytecode_table.index, Rotation::cur()),
            );

            cb.require_equal(
                "cur.value == cur.length",
                meta.query_advice(bytecode_table.value, Rotation::cur()),
                meta.query_advice(length, Rotation::cur()),
            );

            cb.gate(and::expr(vec![
                meta.query_fixed(q_enable, Rotation::cur()),
                not::expr(meta.query_fixed(q_last, Rotation::cur())),
                is_header(meta),
            ]))
        });

        // When is_byte ->
        // assert push_data_size_table_lookup(cur.value, cur.push_data_size)
        // assert cur.is_code == (cur.push_data_left == 0)
        meta.create_gate("Byte row", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_equal(
                "cur.is_code == (cur.push_data_left == 0)",
                meta.query_advice(bytecode_table.is_code, Rotation::cur()),
                push_data_left_is_zero.clone().is_zero_expression,
            );

            cb.gate(and::expr(vec![
                meta.query_fixed(q_enable, Rotation::cur()),
                not::expr(meta.query_fixed(q_last, Rotation::cur())),
                is_byte(meta),
            ]))
        });
        meta.lookup_any(
            "push_data_size_table_lookup(cur.value, cur.push_data_size)",
            |meta| {
                let enable = and::expr(vec![
                    meta.query_fixed(q_enable, Rotation::cur()),
                    not::expr(meta.query_fixed(q_last, Rotation::cur())),
                    is_byte(meta),
                ]);

                let lookup_columns = vec![value, push_data_size];

                let mut constraints = vec![];

                for i in 0..PUSH_TABLE_WIDTH {
                    constraints.push((
                        enable.clone() * meta.query_advice(lookup_columns[i], Rotation::cur()),
                        meta.query_fixed(push_table[i], Rotation::cur()),
                    ))
                }
                constraints
            },
        );

        // When is_header_to_header or q_last ->
        // assert cur.length == 0
        // assert cur.hash == EMPTY_HASH
        meta.create_gate("Header to header row", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_zero(
                "cur.length == 0",
                meta.query_advice(length, Rotation::cur()),
            );

            // TODO: assert cur.hash == EMPTY_HASH
            // FIXME: Since randomness is only known at synthesis time, the RLC of empty
            // code_hash is not constant.  Consider doing a lookup to the empty code_hash
            // value? cb.condition(length_is_zero.clone().is_zero_expression,
            // |cb| {     cb.require_equal(
            //         "if length == 0: code_hash == RLC(EMPTY_HASH, randomness)",
            //         meta.query_advice(bytecode_table.code_hash, Rotation::cur()),
            //         Expression::Constant(keccak(&[], randomness)),
            //     );
            // });

            cb.gate(and::expr(vec![
                meta.query_fixed(q_enable, Rotation::cur()),
                or::expr(vec![
                    is_header_to_header(meta),
                    meta.query_fixed(q_last, Rotation::cur()),
                ]),
            ]))
        });

        // When is_header_to_byte ->
        // assert next.length == cur.length
        // assert next.index == 0
        // assert next.is_code == 1
        // assert next.hash == cur.hash
        // assert next.value_rlc == next.value
        meta.create_gate("Header to byte row", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_equal(
                "next.length == cur.length",
                meta.query_advice(length, Rotation::next()),
                meta.query_advice(length, Rotation::cur()),
            );

            cb.require_zero(
                "next.index == 0",
                meta.query_advice(bytecode_table.index, Rotation::next()),
            );

            cb.require_equal(
                "next.is_code == 1",
                meta.query_advice(bytecode_table.is_code, Rotation::next()),
                1.expr(),
            );

            cb.require_equal(
                "next.hash == cur.hash",
                meta.query_advice(bytecode_table.code_hash, Rotation::next()),
                meta.query_advice(bytecode_table.code_hash, Rotation::cur()),
            );

            cb.require_equal(
                "next.value_rlc == next.value",
                meta.query_advice(value_rlc, Rotation::next()),
                meta.query_advice(bytecode_table.value, Rotation::next()),
            );

            cb.gate(and::expr(vec![
                meta.query_fixed(q_enable, Rotation::cur()),
                not::expr(meta.query_fixed(q_last, Rotation::cur())),
                is_header_to_byte(meta),
            ]))
        });

        // When is_byte_to_byte ->
        // assert next.length == cur.length
        // assert next.index == cur.index + 1
        // assert next.hash == cur.hash
        // assert next.value_rlc == cur.value_rlc * randomness + next.value
        // if cur.is_code:
        //     assert next.push_data_left == cur.push_data_size
        // else:
        //     assert next.push_data_left == cur.push_data_left - 1
        meta.create_gate("Byte to Byte row", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_equal(
                "next.length == cur.length",
                meta.query_advice(length, Rotation::next()),
                meta.query_advice(length, Rotation::cur()),
            );

            cb.require_equal(
                "next.index == cur.index + 1",
                meta.query_advice(bytecode_table.index, Rotation::next()),
                meta.query_advice(bytecode_table.index, Rotation::cur()) + 1.expr(),
            );

            cb.require_equal(
                "next.hash == cur.hash",
                meta.query_advice(bytecode_table.code_hash, Rotation::next()),
                meta.query_advice(bytecode_table.code_hash, Rotation::cur()),
            );

            // TODO: check this
            cb.require_equal(
                "next.value_rlc == cur.value_rlc * randomness + next.value",
                meta.query_advice(value_rlc, Rotation::next()),
                meta.query_advice(value_rlc, Rotation::cur()) * challenges.keccak_input()
                    + meta.query_advice(value, Rotation::cur()),
            );

            cb.require_equal(
                "next.push_data_left == cur.is_code ? cur.push_data_size : cur.push_data_left - 1",
                meta.query_advice(push_data_left, Rotation::next()),
                select::expr(
                    meta.query_advice(bytecode_table.is_code, Rotation::cur()),
                    meta.query_advice(push_data_size, Rotation::cur()),
                    meta.query_advice(push_data_left, Rotation::cur()) - 1.expr(),
                ),
            );

            cb.gate(and::expr(vec![
                meta.query_fixed(q_enable, Rotation::cur()),
                not::expr(meta.query_fixed(q_last, Rotation::cur())),
                is_byte_to_byte(meta),
            ]))
        });

        // When is_byte_to_header ->
        // assert cur.index + 1 == cur.length
        // assert keccak256_table_lookup(cur.hash, cur.length, cur.value_rlc)
        meta.create_gate("Byte to Header row", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_equal(
                "cur.index + 1 == cur.length",
                meta.query_advice(bytecode_table.index, Rotation::next()) + 1.expr(),
                meta.query_advice(length, Rotation::cur()),
            );

            cb.gate(and::expr(vec![
                meta.query_fixed(q_enable, Rotation::cur()),
                not::expr(meta.query_fixed(q_last, Rotation::cur())),
                is_byte_to_header(meta),
            ]))
        });
        meta.lookup_any(
            "keccak256_table_lookup(cur.hash, cur.length, cur.value_rlc)",
            |meta| {
                let enable = and::expr(vec![
                    meta.query_fixed(q_enable, Rotation::cur()),
                    not::expr(meta.query_fixed(q_last, Rotation::cur())),
                    is_byte_to_header(meta),
                ]);

                let lookup_columns = vec![value_rlc, length, bytecode_table.code_hash];

                let mut constraints = vec![(
                    enable.clone(),
                    meta.query_advice(keccak_table.is_enabled, Rotation::cur()),
                )];

                // TODO: perhaps write this explicitly so it is more readable the matching
                // between collumns
                for (i, column) in keccak_table.columns().iter().skip(1).enumerate() {
                    constraints.push((
                        enable.clone() * meta.query_advice(lookup_columns[i], Rotation::cur()),
                        meta.query_advice(*column, Rotation::cur()),
                    ))
                }

                constraints
            },
        );

        BytecodeCircuitConfig {
            minimum_rows: meta.minimum_rows(),
            q_enable,
            q_first,
            q_last,
            bytecode_table,
            push_data_left,
            value_rlc,
            length,
            push_data_size,
            push_data_left_inv,
            push_data_left_is_zero,
            push_table,
            keccak_table,
        }
    }
}

impl<F: Field> BytecodeCircuitConfig<F> {
    pub(crate) fn assign(
        &self,
        layouter: &mut impl Layouter<F>,
        size: usize,
        witness: &[UnrolledBytecode<F>],
        challenges: &Challenges<Value<F>>,
    ) -> Result<(), Error> {
        self.assign_internal(layouter, size, witness, challenges, true)
    }

    pub(crate) fn assign_internal(
        &self,
        layouter: &mut impl Layouter<F>,
        size: usize,
        witness: &[UnrolledBytecode<F>],
        challenges: &Challenges<Value<F>>,
        fail_fast: bool,
    ) -> Result<(), Error> {
        let push_data_left_is_zero_chip =
            IsZeroChip::construct(self.push_data_left_is_zero.clone());

        // Subtract the unusable rows from the size
        assert!(size > self.minimum_rows);
        let last_row_offset = size - self.minimum_rows + 1;

        layouter.assign_region(
            || "assign bytecode",
            |mut region| {
                let mut offset = 0;
                for bytecode in witness.iter() {
                    self.assign_bytecode(
                        &mut region,
                        bytecode,
                        challenges,
                        &push_data_left_is_zero_chip,
                        &mut offset,
                        last_row_offset,
                        fail_fast,
                    )?;
                }

                // Padding
                for idx in offset..=last_row_offset {
                    self.set_padding_row(
                        &mut region,
                        &push_data_left_is_zero_chip,
                        idx,
                        last_row_offset,
                    )?;
                }
                Ok(())
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn assign_bytecode(
        &self,
        region: &mut Region<'_, F>,
        bytecode: &UnrolledBytecode<F>,
        challenges: &Challenges<Value<F>>,
        push_rindex_is_zero_chip: &IsZeroChip<F>,
        offset: &mut usize,
        last_row_offset: usize,
        fail_fast: bool,
    ) -> Result<(), Error> {
        // Run over all the bytes
        let mut push_data_left = 0;
        let mut push_data_size = 0;
        let mut value_rlc = challenges.keccak_input().map(|_| F::zero());
        let length = F::from(bytecode.bytes.len() as u64);

        for (idx, row) in bytecode.rows.iter().enumerate() {
            if fail_fast && *offset > last_row_offset {
                log::error!(
                    "Bytecode Circuit: offset={} > last_row_offset={}",
                    offset,
                    last_row_offset
                );
                return Err(Error::Synthesis);
            }

            // TODO: why different code_hash for each row? Is this going to produce the same
            // result for every row?
            let code_hash = challenges.evm_word().map(|challenge| {
                RandomLinearCombination::<F, 32>::random_linear_combine(
                    row.code_hash.to_le_bytes(),
                    challenge,
                )
            });

            // Track which byte is an opcode and which is push
            // data
            if idx > 0 {
                let is_code = push_data_left == 0;
                assert_eq!(F::from(is_code as u64), row.is_code, "is_code must match");

                push_data_size = get_push_size(row.value.get_lower_128() as u8);

                push_data_left = if is_code {
                    push_data_size
                } else {
                    push_data_left - 1
                };

                value_rlc
                    .as_mut()
                    .zip(challenges.keccak_input())
                    .map(|(value_rlc, challenge)| *value_rlc = *value_rlc * challenge + row.value);
            }

            // Set the data for this row
            if *offset <= last_row_offset {
                self.set_row(
                    region,
                    push_rindex_is_zero_chip,
                    *offset,
                    true,
                    *offset == last_row_offset,
                    code_hash,
                    row.tag,
                    row.index,
                    row.is_code,
                    row.value,
                    push_data_left,
                    value_rlc,
                    length,
                    F::from(push_data_size as u64),
                )?;
                *offset += 1;
            }
        }

        Ok(())
    }

    fn set_padding_row(
        &self,
        region: &mut Region<'_, F>,
        push_data_left_is_zero_chip: &IsZeroChip<F>,
        offset: usize,
        last_row_offset: usize,
    ) -> Result<(), Error> {
        self.set_row(
            region,
            push_data_left_is_zero_chip,
            offset,
            offset < last_row_offset,
            offset == last_row_offset,
            Value::known(F::zero()),
            F::from(BytecodeFieldTag::Header as u64),
            F::zero(),
            F::zero(),
            F::zero(),
            0,
            Value::known(F::zero()),
            F::zero(),
            F::zero(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn set_row(
        &self,
        region: &mut Region<'_, F>,
        push_data_left_is_zero_chip: &IsZeroChip<F>,
        offset: usize,
        enable: bool,
        last: bool,
        code_hash: Value<F>,
        tag: F,
        index: F,
        is_code: F,
        value: F,
        push_data_left: u64,
        value_rlc: Value<F>,
        length: F,
        push_data_size: F,
    ) -> Result<(), Error> {
        // q_enable
        region.assign_fixed(
            || format!("assign q_enable {}", offset),
            self.q_enable,
            offset,
            || Value::known(F::from(enable as u64)),
        )?;

        // q_first
        region.assign_fixed(
            || format!("assign q_first {}", offset),
            self.q_first,
            offset,
            || Value::known(F::from((offset == 0) as u64)),
        )?;

        // q_last
        let q_last_value = if last { F::one() } else { F::zero() };
        region.assign_fixed(
            || format!("assign q_first {}", offset),
            self.q_last,
            offset,
            || Value::known(q_last_value),
        )?;

        // Advices
        for (name, column, value) in [
            ("tag", self.bytecode_table.tag, tag),
            ("index", self.bytecode_table.index, index),
            ("is_code", self.bytecode_table.is_code, is_code),
            ("value", self.bytecode_table.value, value),
            (
                "push_data_left",
                self.push_data_left,
                F::from(push_data_left),
            ),
            ("length", self.length, length),
            ("push_data_size", self.push_data_size, push_data_size),
        ] {
            region.assign_advice(
                || format!("assign {} {}", name, offset),
                column,
                offset,
                || Value::known(value),
            )?;
        }
        for (name, column, value) in [
            ("code_hash", self.bytecode_table.code_hash, code_hash),
            ("value_rlc", self.value_rlc, value_rlc),
        ] {
            region.assign_advice(
                || format!("assign {} {}", name, offset),
                column,
                offset,
                || value,
            )?;
        }

        push_data_left_is_zero_chip.assign(
            region,
            offset,
            Value::known(F::from(push_data_left)),
        )?;

        Ok(())
    }

    /// load fixed tables
    pub(crate) fn load_aux_tables(&self, layouter: &mut impl Layouter<F>) -> Result<(), Error> {
        // push table: BYTE -> NUM_PUSHED:
        // [0, OpcodeId::PUSH1] -> 0
        // [OpcodeId::PUSH1, OpcodeId::PUSH32] -> [1..32]
        // [OpcodeId::PUSH32, 256] -> 0
        layouter.assign_region(
            || "push table",
            |mut region| {
                for byte in 0usize..256 {
                    let push_size = get_push_size(byte as u8);
                    for (name, column, value) in &[
                        ("byte", self.push_table[0], byte as u64),
                        ("push_size", self.push_table[1], push_size),
                    ] {
                        region.assign_fixed(
                            || format!("Push table assign {} {}", name, byte),
                            *column,
                            byte,
                            || Value::known(F::from(*value)),
                        )?;
                    }
                }
                Ok(())
            },
        )?;

        Ok(())
    }
}

/// Get unrolled bytecode from raw bytes
pub fn unroll<F: Field>(bytes: Vec<u8>) -> UnrolledBytecode<F> {
    let code_hash = keccak(&bytes[..]);
    let mut rows = vec![BytecodeRow::<F> {
        code_hash,
        tag: F::from(BytecodeFieldTag::Header as u64),
        index: F::zero(),
        is_code: F::zero(),
        value: F::from(bytes.len() as u64),
    }];
    // Run over all the bytes
    let mut push_rindex = 0;
    for (index, byte) in bytes.iter().enumerate() {
        // Track which byte is an opcode and which is push data
        let is_code = push_rindex == 0;
        push_rindex = if is_code {
            get_push_size(*byte)
        } else {
            push_rindex - 1
        };

        rows.push(BytecodeRow::<F> {
            code_hash,
            tag: F::from(BytecodeFieldTag::Byte as u64),
            index: F::from(index as u64),
            is_code: F::from(is_code as u64),
            value: F::from(*byte as u64),
        });
    }
    UnrolledBytecode { bytes, rows }
}

fn is_push(byte: u8) -> bool {
    OpcodeId::from(byte).is_push()
}

fn get_push_size(byte: u8) -> u64 {
    if is_push(byte) {
        byte as u64 - OpcodeId::PUSH1.as_u64() + 1
    } else {
        0u64
    }
}

fn keccak(msg: &[u8]) -> Word {
    let mut keccak = Keccak::default();
    keccak.update(msg);
    Word::from_big_endian(keccak.digest().as_slice())
}

fn into_words(message: &[u8]) -> Vec<u64> {
    let words_total = message.len() / 8;
    let mut words: Vec<u64> = vec![0; words_total];

    for i in 0..words_total {
        let mut word_bits: [u8; 8] = Default::default();
        word_bits.copy_from_slice(&message[i * 8..i * 8 + 8]);
        words[i] = u64::from_le_bytes(word_bits);
    }

    words
}

/// BytecodeCircuit
#[derive(Clone, Default, Debug)]
pub struct BytecodeCircuit<F: Field> {
    /// Unrolled bytecodes
    pub bytecodes: Vec<UnrolledBytecode<F>>,
    /// Circuit size
    pub size: usize,
}

impl<F: Field> BytecodeCircuit<F> {
    /// new BytecodeCircuitTester
    pub fn new(bytecodes: Vec<UnrolledBytecode<F>>, size: usize) -> Self {
        BytecodeCircuit { bytecodes, size }
    }

    /// Creates bytecode circuit from block and bytecode_size.
    pub fn new_from_block_sized(block: &witness::Block<F>, bytecode_size: usize) -> Self {
        let bytecodes: Vec<UnrolledBytecode<F>> = block
            .bytecodes
            .iter()
            .map(|(_, b)| unroll(b.bytes.clone()))
            .collect();
        Self::new(bytecodes, bytecode_size)
    }
}

impl<F: Field> SubCircuit<F> for BytecodeCircuit<F> {
    type Config = BytecodeCircuitConfig<F>;

    fn new_from_block(block: &witness::Block<F>) -> Self {
        // TODO: Find a nicer way to add the extra `128`.  Is this to account for
        // unusable rows? Then it could be calculated like this:
        // fn unusable_rows<F: Field, C: Circuit<F>>() -> usize {
        //     let mut cs = ConstraintSystem::default();
        //     C::configure(&mut cs);
        //     cs.blinding_factors()
        // }
        let bytecode_size = block.circuits_params.max_bytecode + 128;
        Self::new_from_block_sized(block, bytecode_size)
    }

    /// Return the minimum number of rows required to prove the block
    fn min_num_rows_block(block: &witness::Block<F>) -> usize {
        block
            .bytecodes
            .values()
            .map(|bytecode| bytecode.bytes.len() + 1)
            .sum()
    }

    /// Make the assignments to the TxCircuit
    fn synthesize_sub(
        &self,
        config: &Self::Config,
        challenges: &Challenges<Value<F>>,
        layouter: &mut impl Layouter<F>,
    ) -> Result<(), Error> {
        config.load_aux_tables(layouter)?;
        config.assign_internal(layouter, self.size, &self.bytecodes, challenges, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode_circuit::dev::test_bytecode_circuit_unrolled;
    use eth_types::Bytecode;
    use halo2_proofs::halo2curves::bn256::Fr;

    fn get_randomness<F: Field>() -> F {
        F::from(123456)
    }

    /// Verify unrolling code
    #[test]
    fn bytecode_unrolling() {
        let k = 10;
        let mut rows = vec![];
        let mut bytecode = Bytecode::default();
        // First add all non-push bytes, which should all be seen as code
        for byte in 0u8..=255u8 {
            if !is_push(byte) {
                bytecode.write(byte, true);
                rows.push(BytecodeRow {
                    code_hash: Word::zero(),
                    tag: Fr::from(BytecodeFieldTag::Byte as u64),
                    index: Fr::from(rows.len() as u64),
                    is_code: Fr::from(true as u64),
                    value: Fr::from(byte as u64),
                });
            }
        }
        // Now add the different push ops
        for n in 1..=32 {
            let data_byte = OpcodeId::PUSH32.as_u8();
            bytecode.push(
                n,
                Word::from_little_endian(&vec![data_byte; n as usize][..]),
            );
            rows.push(BytecodeRow {
                code_hash: Word::zero(),
                tag: Fr::from(BytecodeFieldTag::Byte as u64),
                index: Fr::from(rows.len() as u64),
                is_code: Fr::from(true as u64),
                value: Fr::from(OpcodeId::PUSH1.as_u64() + ((n - 1) as u64)),
            });
            for _ in 0..n {
                rows.push(BytecodeRow {
                    code_hash: Word::zero(),
                    tag: Fr::from(BytecodeFieldTag::Byte as u64),
                    index: Fr::from(rows.len() as u64),
                    is_code: Fr::from(false as u64),
                    value: Fr::from(data_byte as u64),
                });
            }
        }
        // Set the code_hash of the complete bytecode in the rows
        let code_hash = keccak(&bytecode.to_vec()[..]);
        for row in rows.iter_mut() {
            row.code_hash = code_hash;
        }
        rows.insert(
            0,
            BytecodeRow {
                code_hash,
                tag: Fr::from(BytecodeFieldTag::Header as u64),
                index: Fr::zero(),
                is_code: Fr::zero(),
                value: Fr::from(bytecode.to_vec().len() as u64),
            },
        );
        // Unroll the bytecode
        let unrolled = unroll(bytecode.to_vec());
        // Check if the bytecode was unrolled correctly
        assert_eq!(
            UnrolledBytecode {
                bytes: bytecode.to_vec(),
                rows,
            },
            unrolled,
        );
        // Verify the unrolling in the circuit
        test_bytecode_circuit_unrolled::<Fr>(k, vec![unrolled], true);
    }

    /// Tests a fully empty circuit
    #[test]
    fn bytecode_empty() {
        let k = 9;
        test_bytecode_circuit_unrolled::<Fr>(k, vec![unroll(vec![])], true);
    }

    #[test]
    fn bytecode_simple() {
        let k = 9;
        let bytecodes = vec![unroll(vec![7u8]), unroll(vec![6u8]), unroll(vec![5u8])];
        test_bytecode_circuit_unrolled::<Fr>(k, bytecodes, true);
    }

    /// Tests a fully full circuit
    #[test]
    fn bytecode_full() {
        let k = 9;
        test_bytecode_circuit_unrolled::<Fr>(k, vec![unroll(vec![7u8; 2usize.pow(k) - 7])], true);
    }

    /// Tests a circuit with incomplete bytecode
    #[test]
    fn bytecode_incomplete() {
        let k = 9;
        test_bytecode_circuit_unrolled::<Fr>(k, vec![unroll(vec![7u8; 2usize.pow(k) + 1])], false);
    }

    /// Tests multiple bytecodes in a single circuit
    #[test]
    fn bytecode_push() {
        let k = 9;
        test_bytecode_circuit_unrolled::<Fr>(
            k,
            vec![
                unroll(vec![]),
                unroll(vec![OpcodeId::PUSH32.as_u8()]),
                unroll(vec![OpcodeId::PUSH32.as_u8(), OpcodeId::ADD.as_u8()]),
                unroll(vec![OpcodeId::ADD.as_u8(), OpcodeId::PUSH32.as_u8()]),
                unroll(vec![
                    OpcodeId::ADD.as_u8(),
                    OpcodeId::PUSH32.as_u8(),
                    OpcodeId::ADD.as_u8(),
                ]),
            ],
            true,
        );
    }

    /// Test invalid code_hash data
    #[test]
    fn bytecode_invalid_hash_data() {
        let k = 9;
        let bytecode = vec![8u8, 2, 3, 8, 9, 7, 128];
        let unrolled = unroll(bytecode);
        test_bytecode_circuit_unrolled::<Fr>(k, vec![unrolled.clone()], true);
        // Change the code_hash on the first position
        {
            let mut invalid = unrolled.clone();
            invalid.rows[0].code_hash += Word::one();
            test_bytecode_circuit_unrolled::<Fr>(k, vec![invalid], false);
        }
        // Change the code_hash on another position
        {
            let mut invalid = unrolled.clone();
            invalid.rows[4].code_hash += Word::one();
            test_bytecode_circuit_unrolled::<Fr>(k, vec![invalid], false);
        }
        // Change all the hashes so it doesn't match the keccak lookup code_hash
        {
            let mut invalid = unrolled;
            for row in invalid.rows.iter_mut() {
                row.code_hash = Word::one();
            }
            test_bytecode_circuit_unrolled::<Fr>(k, vec![invalid], false);
        }
    }

    /// Test invalid index
    #[test]
    #[ignore]
    fn bytecode_invalid_index() {
        let k = 9;
        let bytecode = vec![8u8, 2, 3, 8, 9, 7, 128];
        let unrolled = unroll(bytecode);
        test_bytecode_circuit_unrolled::<Fr>(k, vec![unrolled.clone()], true);
        // Start the index at 1
        {
            let mut invalid = unrolled.clone();
            for row in invalid.rows.iter_mut() {
                row.index += Fr::one();
            }
            test_bytecode_circuit_unrolled::<Fr>(k, vec![invalid], false);
        }
        // Don't increment an index once
        {
            let mut invalid = unrolled;
            invalid.rows.last_mut().unwrap().index -= Fr::one();
            test_bytecode_circuit_unrolled::<Fr>(k, vec![invalid], false);
        }
    }

    /// Test invalid byte data
    #[test]
    fn bytecode_invalid_byte_data() {
        let k = 9;
        let bytecode = vec![8u8, 2, 3, 8, 9, 7, 128];
        let unrolled = unroll(bytecode);
        test_bytecode_circuit_unrolled::<Fr>(k, vec![unrolled.clone()], true);
        // Change the first byte
        {
            let mut invalid = unrolled.clone();
            invalid.rows[1].value = Fr::from(9u64);
            test_bytecode_circuit_unrolled::<Fr>(k, vec![invalid], false);
        }
        // Change a byte on another position
        {
            let mut invalid = unrolled.clone();
            invalid.rows[5].value = Fr::from(6u64);
            test_bytecode_circuit_unrolled::<Fr>(k, vec![invalid], false);
        }
        // Set a byte value out of range
        {
            let mut invalid = unrolled;
            invalid.rows[3].value = Fr::from(256u64);
            test_bytecode_circuit_unrolled::<Fr>(k, vec![invalid], false);
        }
    }

    /// Test invalid is_code data
    #[test]
    fn bytecode_invalid_is_code() {
        let k = 9;
        let bytecode = vec![
            OpcodeId::ADD.as_u8(),
            OpcodeId::PUSH1.as_u8(),
            OpcodeId::PUSH1.as_u8(),
            OpcodeId::SUB.as_u8(),
            OpcodeId::PUSH7.as_u8(),
            OpcodeId::ADD.as_u8(),
            OpcodeId::PUSH6.as_u8(),
        ];
        let unrolled = unroll(bytecode);
        test_bytecode_circuit_unrolled::<Fr>(k, vec![unrolled.clone()], true);
        // Mark the 3rd byte as code (is push data from the first PUSH1)
        {
            let mut invalid = unrolled.clone();
            invalid.rows[3].is_code = Fr::one();
            test_bytecode_circuit_unrolled::<Fr>(k, vec![invalid], false);
        }
        // Mark the 4rd byte as data (is code)
        {
            let mut invalid = unrolled.clone();
            invalid.rows[4].is_code = Fr::zero();
            test_bytecode_circuit_unrolled::<Fr>(k, vec![invalid], false);
        }
        // Mark the 7th byte as code (is data for the PUSH7)
        {
            let mut invalid = unrolled;
            invalid.rows[7].is_code = Fr::one();
            test_bytecode_circuit_unrolled::<Fr>(k, vec![invalid], false);
        }
    }
}
