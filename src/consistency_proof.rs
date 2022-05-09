//! Types and traits for Merkle consistency proofs

use crate::{
    leaf::CanonicalSerialize,
    merkle_tree::{parent_hash, CtMerkleTree, RootHash},
    tree_math::*,
};

use core::marker::PhantomData;
use std::io::Error as IoError;

use digest::{typenum::Unsigned, Digest};
use thiserror::Error;

/// An error representing what went wrong during membership verification
#[derive(Debug, Error)]
pub enum VerificationError {
    /// An error occurred when serializing the item whose memberhsip is being checked
    #[error("could not canonically serialize a item")]
    Io(#[from] IoError),

    /// The proof is malformed
    #[error("proof size is not a multiple of the hash digest size")]
    MalformedProof,

    /// The provided root hash does not match the proof's root hash w.r.t the item
    #[error("memberhsip verificaiton failed")]
    Failure,
}

#[derive(Clone, Debug)]
pub struct ConsistencyProof<H: Digest> {
    proof: Vec<u8>,
    _marker: PhantomData<H>,
}

/// A reference to a [`ConsistencyProof`]
#[derive(Clone, Debug)]
pub struct ConsistencyProofRef<'a, H: Digest> {
    proof: &'a [u8],
    _marker: PhantomData<H>,
}

impl<H: Digest> ConsistencyProof<H> {
    pub fn as_ref(&self) -> ConsistencyProofRef<H> {
        ConsistencyProofRef {
            proof: self.proof.as_slice(),
            _marker: self._marker,
        }
    }

    /// Returns the RFC 6962-compatible byte representation of this membership proof
    pub fn as_bytes(&self) -> &[u8] {
        self.proof.as_slice()
    }

    /// Constructs a `ConsistencyProof` from the given bytes. Panics when `bytes.len()` is not a
    /// multiple of `H::OutputSize::USIZE`, i.e., when `bytes` is not a concatenated sequence of
    /// hash digests.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        if bytes.len() % H::OutputSize::USIZE != 0 {
            panic!("malformed consistency proof");
        } else {
            ConsistencyProof {
                proof: bytes.to_vec(),
                _marker: PhantomData,
            }
        }
    }
}

impl<H, T> CtMerkleTree<H, T>
where
    H: Digest,
    T: CanonicalSerialize,
{
    /// Produces a proof that this `CtMerkleTree` is the result of appending to a tree with the
    /// same `subslice_size` initial elements. Panics if `subslice_size == 0`.
    pub fn consistency_proof(&self, subslice_size: usize) -> ConsistencyProof<H> {
        if subslice_size == 0 {
            panic!("cannot produce a consistency proof starting from an empty tree");
        }

        let num_tree_leaves = self.leaves.len() as u64;
        let num_oldtree_leaves = subslice_size as u64;
        let tree_root_idx = root_idx(num_tree_leaves as u64);
        let oldtree_root_idx = root_idx(num_oldtree_leaves as u64);
        let starting_idx: InternalIdx = LeafIdx::new(subslice_size as u64 - 1).into();

        // A consistency proof from self to self is empty
        if subslice_size == num_tree_leaves as usize {
            return ConsistencyProof {
                proof: Vec::new(),
                _marker: PhantomData,
            };
        }

        // We have starting_idx in a current tree and a old tree. starting_idx occurs in a subtree
        // which is both a subtree of the current tree and of the old tree.
        // We want to find the largest such subtree, and start logging the copath after that.

        let mut proof = Vec::new();

        // We have a special case when the old tree is a subtree
        let oldtree_is_subtree = subslice_size.is_power_of_two();

        // If the old tree isn't a subtree, find the first place that the ancestors of the starting
        // index diverge
        let mut path_idx = if !oldtree_is_subtree {
            let mut ancestor_in_tree = starting_idx;
            let mut ancestor_in_oldtree = starting_idx;

            // We don't have to worry about this panicking. ancestor_in_oldtree can only ever be
            // the root idx if oldtree_is_subtree.
            while ancestor_in_tree.parent(num_tree_leaves)
                == ancestor_in_oldtree.parent(num_oldtree_leaves)
            {
                // Step up the trees
                ancestor_in_tree = ancestor_in_tree.parent(num_tree_leaves);
                ancestor_in_oldtree = ancestor_in_oldtree.parent(num_oldtree_leaves);
            }

            // We found the divergent point. Record the point just before divergences
            println!("Adding index {} to proof", ancestor_in_tree.usize());
            proof.extend_from_slice(&self.internal_nodes[ancestor_in_tree.usize()]);

            ancestor_in_tree
        } else {
            oldtree_root_idx
        };

        // Now collect the copath, just like in the membership proof
        while path_idx != tree_root_idx {
            let sibling_idx = path_idx.sibling(num_tree_leaves);
            println!("Adding index {} to proof", sibling_idx.usize());
            proof.extend_from_slice(&self.internal_nodes[sibling_idx.usize()]);

            // Go up a level
            path_idx = path_idx.parent(num_tree_leaves);
        }

        ConsistencyProof {
            proof,
            _marker: PhantomData,
        }
    }
}

impl<H: Digest> RootHash<H> {
    /// Verifies that `val` occurs at index `idx` in the tree described by this `RootHash`. Panics
    /// if `old_root` represents the empty tree.
    pub fn verify_consistency(
        &self,
        old_root: &RootHash<H>,
        proof: &ConsistencyProofRef<H>,
    ) -> Result<(), VerificationError> {
        let starting_idx: InternalIdx = LeafIdx::new(old_root.num_leaves - 1).into();
        let num_tree_leaves = self.num_leaves;
        let num_oldtree_leaves = old_root.num_leaves;
        let oldtree_root_idx = root_idx(num_oldtree_leaves);

        if num_oldtree_leaves == 0 {
            panic!("consistency proofs cannot exist wrt the empty tree");
        }

        // The update was from self to self. Nothing changed, and the proof is empty. Success.
        if old_root.root_hash == self.root_hash && proof.proof.len() == 0 {
            return Ok(());
        }

        // We have a special case when the old tree is a subtree
        let oldtree_is_subtree = old_root.num_leaves.is_power_of_two();

        let mut digests = proof
            .proof
            .chunks(H::OutputSize::USIZE)
            .map(digest::Output::<H>::from_slice);

        // We compute both old and new tree hashes. This procedure will succeed iff the oldtree
        // hash matches old_root and the tree hash matches self
        let (mut running_oldtree_idx, mut running_oldtree_hash) = if oldtree_is_subtree {
            (oldtree_root_idx, old_root.root_hash.clone())
        } else {
            // If the old tree isn't a subtree, find the first place that the ancestors of the
            // starting index diverge
            let mut ancestor_in_tree = starting_idx;
            let mut ancestor_in_oldtree = starting_idx;

            // We don't have to worry about this panicking. ancestor_in_oldtree can only ever be
            // the root idx if oldtree_is_subtree.
            while ancestor_in_tree.parent(num_tree_leaves)
                == ancestor_in_oldtree.parent(num_oldtree_leaves)
            {
                // Step up the trees
                ancestor_in_tree = ancestor_in_tree.parent(num_tree_leaves);
                ancestor_in_oldtree = ancestor_in_oldtree.parent(num_oldtree_leaves);
            }

            // We found the divergent point. Record the point just before divergences
            (ancestor_in_tree, digests.next().unwrap().clone())
        };
        let mut running_tree_hash = running_oldtree_hash.clone();
        let mut running_tree_idx = running_oldtree_idx;

        for sibling_hash in digests {
            let sibling_idx = running_tree_idx.sibling(num_tree_leaves);

            println!(
                "Tree: {} <-> {}",
                running_tree_idx.usize(),
                sibling_idx.usize()
            );

            if running_tree_idx.is_left(num_tree_leaves) {
                running_tree_hash = parent_hash::<H>(&running_tree_hash, sibling_hash);
            } else {
                running_tree_hash = parent_hash::<H>(sibling_hash, &running_tree_hash);
            }
            // Step up the tree
            running_tree_idx = running_tree_idx.parent(num_tree_leaves);

            // Now do the same with the old tree. If the current copath node is the sibling of
            // running_oldtree_idx, then we can update the oldtree hash
            if running_oldtree_idx != oldtree_root_idx
                && sibling_idx == running_oldtree_idx.sibling(num_oldtree_leaves)
            {
                println!(
                    "Oldtree: {} <-> {}",
                    running_tree_idx.usize(),
                    sibling_idx.usize()
                );
                println!("Updating old tree hash");
                if running_oldtree_idx.is_left(num_oldtree_leaves) {
                    running_oldtree_hash = parent_hash::<H>(&running_oldtree_hash, sibling_hash);
                } else {
                    running_oldtree_hash = parent_hash::<H>(sibling_hash, &running_oldtree_hash);
                }
                // Step up the oldtree
                running_oldtree_idx = running_oldtree_idx.parent(num_oldtree_leaves);
            }
        }

        // At the end, the old hash should be the old root, and the new hash should be the new root
        if (running_oldtree_hash != old_root.root_hash) || (running_tree_hash != self.root_hash) {
            eprintln!(
                "oldtree match: {}",
                running_oldtree_hash == old_root.root_hash
            );
            eprintln!("tree match: {}", running_tree_hash == self.root_hash);
            Err(VerificationError::Failure)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
pub(crate) mod test {
    use crate::merkle_tree::test::{rand_tree, rand_val};

    use rand::thread_rng;

    // Tests that an honestly generated membership proof verifies
    #[test]
    fn consistency_proof_correctness() {
        let mut rng = thread_rng();

        for initial_size in 1..50 {
            for num_to_add in 0..50 {
                print!(
                    "Consistency check failed for {} -> {} leaves",
                    initial_size,
                    initial_size + num_to_add
                );

                let mut v = rand_tree(&mut rng, initial_size);
                let initial_size = v.len();
                let initial_root = v.root();

                // Now add to v
                for _ in 0..num_to_add {
                    let val = rand_val(&mut rng);
                    v.push(val).unwrap();
                }
                let new_root = v.root();

                // Now make a consistency proof and check it
                let proof = v.consistency_proof(initial_size);
                println!("proof is {} long", proof.proof.len() / 32);
                new_root
                    .verify_consistency(&initial_root, &proof.as_ref())
                    .expect(&format!(
                        "Consistency check failed for {} -> {} leaves",
                        initial_size,
                        initial_size + num_to_add
                    ));
            }
        }
    }
}