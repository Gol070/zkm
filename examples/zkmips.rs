use elf::{endian::AnyEndian, ElfBytes};
use std::env;
use std::fs;
use std::time::Duration;

use plonky2::field::goldilocks_field::GoldilocksField;
use plonky2::plonk::config::{GenericConfig, PoseidonGoldilocksConfig};
use plonky2::util::timing::TimingTree;

use mips_circuits::all_stark::AllStark;
use mips_circuits::config::StarkConfig;
use mips_circuits::cpu::kernel::assembler::segment_kernel;
use mips_circuits::fixed_recursive_verifier::AllRecursiveCircuits;
use mips_circuits::mips_emulator::state::{InstrumentedState, State, SEGMENT_STEPS};
use mips_circuits::mips_emulator::utils::get_block_path;
use mips_circuits::proof;
use mips_circuits::proof::PublicValues;
use mips_circuits::prover::prove;
use mips_circuits::verifier::verify_proof;

fn split_elf_into_segs() {
    // 1. split ELF into segs
    let basedir = env::var("BASEDIR").unwrap_or("/tmp/cannon".to_string());
    let elf_path = env::var("ELF_PATH").expect("ELF file is missing");
    let block_no = env::var("BLOCK_NO").expect("Block number is missing");
    let seg_path = env::var("SEG_OUTPUT").expect("Segment output path is missing");
    let seg_size = env::var("SEG_SIZE").unwrap_or(format!("{SEGMENT_STEPS}"));
    let seg_size = seg_size.parse::<_>().unwrap_or(SEGMENT_STEPS);

    let data = fs::read(elf_path).expect("could not read file");
    let file =
        ElfBytes::<AnyEndian>::minimal_parse(data.as_slice()).expect("opening elf file failed");
    let (mut state, _) = State::load_elf(&file);
    state.patch_go(&file);
    state.patch_stack("");

    let block_path = get_block_path(&basedir, &block_no, "");
    state.load_input(&block_path);

    let mut instrumented_state = InstrumentedState::new(state, block_path);
    instrumented_state.split_segment(false, &seg_path);
    let mut segment_step: usize = seg_size;
    loop {
        if instrumented_state.state.exited {
            break;
        }
        instrumented_state.step();
        segment_step -= 1;
        if segment_step == 0 {
            segment_step = seg_size;
            instrumented_state.split_segment(true, &seg_path);
        }
    }

    instrumented_state.split_segment(true, &seg_path);
    log::info!("Split done");
}

fn prove_single_seg() {
    let basedir = env::var("BASEDIR").unwrap_or("/tmp/cannon".to_string());
    let block = env::var("BLOCK_NO").expect("Block number is missing");
    let file = env::var("BLOCK_FILE").unwrap_or(String::from(""));
    let seg_file = env::var("SEG_FILE").expect("Segment file is missing");
    let seg_size = env::var("SEG_SIZE").unwrap_or(format!("{SEGMENT_STEPS}"));
    let seg_size = seg_size.parse::<_>().unwrap_or(SEGMENT_STEPS);
    let kernel = segment_kernel(&basedir, &block, &file, &seg_file, seg_size);

    const D: usize = 2;
    type C = PoseidonGoldilocksConfig;
    type F = <C as GenericConfig<D>>::F;

    let allstark: AllStark<F, D> = AllStark::default();
    let config = StarkConfig::standard_fast_config();
    let mut timing = TimingTree::new("prove", log::Level::Debug);
    let allproof: proof::AllProof<GoldilocksField, C, D> =
        prove(&allstark, &kernel, &config, &mut timing).unwrap();
    let mut count_bytes = 0;
    let mut row = 0;
    for proof in allproof.stark_proofs.clone() {
        let proof_str = serde_json::to_string(&proof.proof).unwrap();
        log::info!("row:{} proof bytes:{}", row, proof_str.len());
        row = row + 1;
        count_bytes = count_bytes + proof_str.len();
    }
    log::info!("total proof bytes:{}KB", count_bytes / 1024);
    verify_proof(&allstark, allproof, &config).unwrap();
    log::info!("Prove done");
}

fn prove_groth16() {
    todo!()
}

fn main() {
    env_logger::try_init().unwrap_or_default();
    let args: Vec<String> = env::args().collect();
    let helper = || {
        println!(
            "Help: {} split | prove | aggregate_proof | prove_groth16",
            args[0]
        );
        std::process::exit(-1);
    };
    if args.len() < 2 {
        helper();
    }
    match args[1].as_str() {
        "split" => split_elf_into_segs(),
        "prove" => prove_single_seg(),
        "aggregate_proof" => aggregate_proof().unwrap(),
        "prove_groth16" => prove_groth16(),
        _ => helper(),
    };
}

fn aggregate_proof() -> anyhow::Result<()> {
    type F = GoldilocksField;
    const D: usize = 2;
    type C = PoseidonGoldilocksConfig;

    let basedir = env::var("BASEDIR").unwrap_or("/tmp/cannon".to_string());
    let block = env::var("BLOCK_NO").expect("Block number is missing");
    let file = env::var("BLOCK_FILE").unwrap_or(String::from(""));
    let seg_file = env::var("SEG_FILE").expect("first segment file is missing");
    let seg_file2 = env::var("SEG_FILE2").expect("The next segment file is missing");
    let seg_size = env::var("SEG_SIZE").unwrap_or(format!("{SEGMENT_STEPS}"));
    let seg_size = seg_size.parse::<_>().unwrap_or(SEGMENT_STEPS);

    let all_stark = AllStark::<F, D>::default();
    let config = StarkConfig::standard_fast_config();
    // Preprocess all circuits.
    let all_circuits =
        AllRecursiveCircuits::<F, C, D>::new(&all_stark, &[10..12, 10..12, 8..12, 10..13], &config);

    let input_first = segment_kernel(&basedir, &block, &file, &seg_file, seg_size);
    let mut timing = TimingTree::new("prove root first", log::Level::Info);
    let (root_proof_first, first_public_values) =
        all_circuits.prove_root(&all_stark, &input_first, &config, &mut timing)?;

    timing.filter(Duration::from_millis(100)).print();
    all_circuits.verify_root(root_proof_first.clone())?;

    let input = segment_kernel(&basedir, &block, &file, &seg_file2, seg_size);
    let mut timing = TimingTree::new("prove root second", log::Level::Info);
    let (root_proof, public_values) =
        all_circuits.prove_root(&all_stark, &input, &config, &mut timing)?;
    timing.filter(Duration::from_millis(100)).print();

    all_circuits.verify_root(root_proof.clone())?;

    // Update public values for the aggregation.
    let agg_public_values = PublicValues {
        roots_before: first_public_values.roots_before,
        roots_after: public_values.roots_after,
    };

    // We can duplicate the proofs here because the state hasn't mutated.
    let (agg_proof, updated_agg_public_values) = all_circuits.prove_aggregation(
        false,
        &root_proof_first,
        false,
        &root_proof,
        agg_public_values,
    )?;
    all_circuits.verify_aggregation(&agg_proof)?;
    let (block_proof, _block_public_values) =
        all_circuits.prove_block(None, &agg_proof, updated_agg_public_values)?;

    log::info!(
        "proof size: {:?}",
        serde_json::to_string(&block_proof.proof).unwrap().len()
    );
    all_circuits.verify_block(&block_proof)
}