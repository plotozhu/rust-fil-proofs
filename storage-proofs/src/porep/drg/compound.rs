use std::marker::PhantomData;

use anyhow::{ensure, Context};
use bellperson::Circuit;
use fil_sapling_crypto::jubjub::JubjubEngine;
use generic_array::typenum;
use paired::bls12_381::{Bls12, Fr};

use crate::compound_proof::{CircuitComponent, CompoundProof};
use crate::crypto::pedersen::JJ_PARAMS;
use crate::drgraph::Graph;
use crate::error::Result;
use crate::gadgets::por::PoRCompound;
use crate::gadgets::variables::Root;
use crate::hasher::Hasher;
use crate::parameter_cache::{CacheableParameters, ParameterSetMetadata};
use crate::por;
use crate::porep::drg::DrgPoRep;
use crate::proof::ProofScheme;

use super::circuit::DrgPoRepCircuit;

/// DRG based Proof of Replication.
///
/// # Fields
///
/// * `params` - parameters for the curve
///
/// ----> Private `replica_node` - The replica node being proven.
///
/// * `replica_node` - The replica node being proven.
/// * `replica_node_path` - The path of the replica node being proven.
/// * `replica_root` - The merkle root of the replica.
///
/// * `replica_parents` - A list of all parents in the replica, with their value.
/// * `replica_parents_paths` - A list of all parents paths in the replica.
///
/// ----> Private `data_node` - The data node being proven.
///
/// * `data_node_path` - The path of the data node being proven.
/// * `data_root` - The merkle root of the data.
/// * `replica_id` - The id of the replica.
///

pub struct DrgPoRepCompound<H, G>
where
    H: Hasher,
    G::Key: AsRef<H::Domain>,
    G: Graph<H>,
{
    // Sad phantom is sad
    _h: PhantomData<H>,
    _g: PhantomData<G>,
}

impl<E: JubjubEngine, C: Circuit<E>, H: Hasher, G: Graph<H>, P: ParameterSetMetadata>
    CacheableParameters<E, C, P> for DrgPoRepCompound<H, G>
where
    G::Key: AsRef<H::Domain>,
{
    fn cache_prefix() -> String {
        format!("drg-proof-of-replication-{}", H::name())
    }
}

impl<'a, H, G> CompoundProof<'a, Bls12, DrgPoRep<'a, H, G>, DrgPoRepCircuit<'a, H>>
    for DrgPoRepCompound<H, G>
where
    H: 'a + Hasher,
    G::Key: AsRef<H::Domain>,
    G: 'a + Graph<H> + ParameterSetMetadata + Sync + Send,
{
    fn generate_public_inputs(
        pub_in: &<DrgPoRep<'a, H, G> as ProofScheme<'a>>::PublicInputs,
        pub_params: &<DrgPoRep<'a, H, G> as ProofScheme<'a>>::PublicParams,
        // We can ignore k because challenges are generated by caller and included
        // in PublicInputs.
        _k: Option<usize>,
    ) -> Result<Vec<Fr>> {
        let replica_id = pub_in.replica_id.context("missing replica id")?;
        let challenges = &pub_in.challenges;

        ensure!(
            pub_in.tau.is_none() == pub_params.private,
            "Public input parameter tau must be unset"
        );

        let (comm_r, comm_d) = match pub_in.tau {
            None => (None, None),
            Some(tau) => (Some(tau.comm_r), Some(tau.comm_d)),
        };

        let leaves = pub_params.graph.size();

        let por_pub_params = por::PublicParams {
            leaves,
            private: pub_params.private,
        };

        let mut input: Vec<Fr> = Vec::new();
        input.push(replica_id.into());

        let mut parents = vec![0; pub_params.graph.degree()];
        for challenge in challenges {
            let mut por_nodes = vec![*challenge as u32];
            pub_params.graph.parents(*challenge, &mut parents)?;
            por_nodes.extend_from_slice(&parents);

            for node in por_nodes {
                let por_pub_inputs = por::PublicInputs {
                    commitment: comm_r,
                    challenge: node as usize,
                };
                let por_inputs = PoRCompound::<H, typenum::U2>::generate_public_inputs(
                    &por_pub_inputs,
                    &por_pub_params,
                    None,
                )?;

                input.extend(por_inputs);
            }

            let por_pub_inputs = por::PublicInputs {
                commitment: comm_d,
                challenge: *challenge,
            };

            let por_inputs = PoRCompound::<H, typenum::U2>::generate_public_inputs(
                &por_pub_inputs,
                &por_pub_params,
                None,
            )?;
            input.extend(por_inputs);
        }
        Ok(input)
    }

    fn circuit(
        public_inputs: &<DrgPoRep<'a, H, G> as ProofScheme<'a>>::PublicInputs,
        component_private_inputs: <DrgPoRepCircuit<'a, H> as CircuitComponent>::ComponentPrivateInputs,
        proof: &<DrgPoRep<'a, H, G> as ProofScheme<'a>>::Proof,
        public_params: &<DrgPoRep<'a, H, G> as ProofScheme<'a>>::PublicParams,
    ) -> Result<DrgPoRepCircuit<'a, H>> {
        let challenges = public_params.challenges_count;
        let len = proof.nodes.len();

        ensure!(len <= challenges, "too many challenges");
        ensure!(
            proof.replica_parents.len() == len,
            "Number of replica parents must match"
        );
        ensure!(
            proof.replica_nodes.len() == len,
            "Number of replica nodes must match"
        );

        let replica_nodes: Vec<_> = proof
            .replica_nodes
            .iter()
            .map(|node| Some(node.data.into()))
            .collect();

        let replica_nodes_paths: Vec<_> = proof
            .replica_nodes
            .iter()
            .map(|node| node.proof.as_options())
            .collect();

        let is_private = public_params.private;

        let (data_root, replica_root) = if is_private {
            (
                component_private_inputs.comm_d.context("is_private")?,
                component_private_inputs.comm_r.context("is_private")?,
            )
        } else {
            (
                Root::Val(Some(proof.data_root.into())),
                Root::Val(Some(proof.replica_root.into())),
            )
        };

        let replica_id = public_inputs.replica_id;

        let replica_parents: Vec<_> = proof
            .replica_parents
            .iter()
            .map(|parents| {
                parents
                    .iter()
                    .map(|(_, parent)| Some(parent.data.into()))
                    .collect()
            })
            .collect();

        let replica_parents_paths: Vec<Vec<_>> = proof
            .replica_parents
            .iter()
            .map(|parents| {
                let p: Vec<_> = parents
                    .iter()
                    .map(|(_, parent)| parent.proof.as_options())
                    .collect();
                p
            })
            .collect();

        let data_nodes: Vec<_> = proof
            .nodes
            .iter()
            .map(|node| Some(node.data.into()))
            .collect();

        let data_nodes_paths: Vec<_> = proof
            .nodes
            .iter()
            .map(|node| node.proof.as_options())
            .collect();

        ensure!(
            public_inputs.tau.is_none() == public_params.private,
            "inconsistent private state"
        );

        Ok(DrgPoRepCircuit {
            params: &*JJ_PARAMS,
            replica_nodes,
            replica_nodes_paths,
            replica_root,
            replica_parents,
            replica_parents_paths,
            data_nodes,
            data_nodes_paths,
            data_root,
            replica_id: replica_id.map(Into::into),
            private: public_params.private,
            _h: Default::default(),
        })
    }

    fn blank_circuit(
        public_params: &<DrgPoRep<'a, H, G> as ProofScheme<'a>>::PublicParams,
    ) -> DrgPoRepCircuit<'a, H> {
        let depth = public_params.graph.merkle_tree_depth::<typenum::U2>() as usize;
        let degree = public_params.graph.degree();
        let arity = 2;

        let challenges_count = public_params.challenges_count;

        let replica_nodes = vec![None; challenges_count];
        let replica_nodes_paths =
            vec![vec![(vec![None; arity - 1], None); depth - 1]; challenges_count];

        let replica_root = Root::Val(None);
        let replica_parents = vec![vec![None; degree]; challenges_count];
        let replica_parents_paths =
            vec![vec![vec![(vec![None; arity - 1], None); depth - 1]; degree]; challenges_count];
        let data_nodes = vec![None; challenges_count];
        let data_nodes_paths =
            vec![vec![(vec![None; arity - 1], None); depth - 1]; challenges_count];
        let data_root = Root::Val(None);

        DrgPoRepCircuit {
            params: &*JJ_PARAMS,
            replica_nodes,
            replica_nodes_paths,
            replica_root,
            replica_parents,
            replica_parents_paths,
            data_nodes,
            data_nodes_paths,
            data_root,
            replica_id: None,
            private: public_params.private,
            _h: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cache_key::CacheKey;
    use crate::compound_proof;
    use crate::drgraph::{new_seed, BucketGraph, BASE_DEGREE};
    use crate::fr32::fr_into_bytes;
    use crate::gadgets::{MetricCS, TestConstraintSystem};
    use crate::hasher::{Hasher, PedersenHasher, PoseidonHasher};
    use crate::porep::stacked::BINARY_ARITY;
    use crate::porep::{drg, PoRep};
    use crate::proof::NoRequirements;

    use ff::Field;
    use merkletree::store::StoreConfig;
    use pretty_assertions::assert_eq;
    use rand::SeedableRng;
    use rand_xorshift::XorShiftRng;

    #[test]
    #[ignore] // Slow test – run only when compiled for release.
    fn test_drgporep_compound_pedersen() {
        drgporep_test_compound::<PedersenHasher>();
    }

    #[test]
    #[ignore] // Slow test – run only when compiled for release.
    fn test_drgporep_compound_poseidon() {
        drgporep_test_compound::<PoseidonHasher>();
    }

    fn drgporep_test_compound<H: Hasher>() {
        // femme::pretty::Logger::new()
        //     .start(log::LevelFilter::Trace)
        //     .ok();

        let rng = &mut XorShiftRng::from_seed(crate::TEST_SEED);

        let nodes = 8;
        let degree = BASE_DEGREE;
        let challenges = vec![1, 3];

        let replica_id: Fr = Fr::random(rng);
        let mut data: Vec<u8> = (0..nodes)
            .flat_map(|_| fr_into_bytes::<Bls12>(&Fr::random(rng)))
            .collect();

        // Only generate seed once. It would be bad if we used different seeds in the same test.
        let seed = new_seed();

        let setup_params = compound_proof::SetupParams {
            vanilla_params: drg::SetupParams {
                drg: drg::DrgParams {
                    nodes,
                    degree,
                    expansion_degree: 0,
                    seed,
                },
                private: false,
                challenges_count: 2,
            },
            partitions: None,
            priority: false,
        };

        let public_params =
            DrgPoRepCompound::<H, BucketGraph<_>>::setup(&setup_params).expect("setup failed");

        // MT for original data is always named tree-d, and it will be
        // referenced later in the process as such.
        let cache_dir = tempfile::tempdir().unwrap();
        let config = StoreConfig::new(
            cache_dir.path(),
            CacheKey::CommDTree.to_string(),
            StoreConfig::default_cached_above_base_layer(nodes, BINARY_ARITY),
        );

        // Generate a replica path.
        let temp_dir = tempdir::TempDir::new("drgporep-test-compound").unwrap();
        let temp_path = temp_dir.path();
        let replica_path = temp_path.join("replica-path");

        let (tau, aux) = drg::DrgPoRep::<H, _>::replicate(
            &public_params.vanilla_params,
            &replica_id.into(),
            (&mut data[..]).into(),
            None,
            config,
            replica_path.clone(),
        )
        .expect("failed to replicate");

        let public_inputs = drg::PublicInputs::<H::Domain> {
            replica_id: Some(replica_id.into()),
            challenges,
            tau: Some(tau),
        };
        let private_inputs = drg::PrivateInputs {
            tree_d: &aux.tree_d,
            tree_r: &aux.tree_r,
            tree_r_config_levels: StoreConfig::default_cached_above_base_layer(nodes, BINARY_ARITY),
        };

        // This duplication is necessary so public_params don't outlive public_inputs and private_inputs.
        let setup_params = compound_proof::SetupParams {
            vanilla_params: drg::SetupParams {
                drg: drg::DrgParams {
                    nodes,
                    degree,
                    expansion_degree: 0,
                    seed,
                },
                private: false,
                challenges_count: 2,
            },
            partitions: None,
            priority: false,
        };

        let public_params =
            DrgPoRepCompound::<H, BucketGraph<_>>::setup(&setup_params).expect("setup failed");

        {
            let (circuit, inputs) = DrgPoRepCompound::<H, _>::circuit_for_test(
                &public_params,
                &public_inputs,
                &private_inputs,
            )
            .unwrap();

            let mut cs = TestConstraintSystem::new();

            circuit
                .synthesize(&mut cs)
                .expect("failed to synthesize test circuit");
            assert!(cs.is_satisfied());
            assert!(cs.verify(&inputs));

            let blank_circuit = <DrgPoRepCompound<_, _> as CompoundProof<_, _, _>>::blank_circuit(
                &public_params.vanilla_params,
            );

            let mut cs_blank = MetricCS::new();
            blank_circuit
                .synthesize(&mut cs_blank)
                .expect("failed to synthesize blank circuit");

            let a = cs_blank.pretty_print_list();
            let b = cs.pretty_print_list();

            for (i, (a, b)) in a.chunks(100).zip(b.chunks(100)).enumerate() {
                assert_eq!(a, b, "failed at chunk {}", i);
            }
        }

        {
            let gparams =
                DrgPoRepCompound::<H, _>::groth_params(Some(rng), &public_params.vanilla_params)
                    .expect("failed to get groth params");

            let proof = DrgPoRepCompound::<H, _>::prove(
                &public_params,
                &public_inputs,
                &private_inputs,
                &gparams,
            )
            .expect("failed while proving");

            let verified = DrgPoRepCompound::<H, _>::verify(
                &public_params,
                &public_inputs,
                &proof,
                &NoRequirements,
            )
            .expect("failed while verifying");

            assert!(verified);
        }
    }
}
