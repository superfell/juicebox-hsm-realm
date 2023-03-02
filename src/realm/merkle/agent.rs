use async_trait::async_trait;
use std::collections::HashMap;

use hsmcore::bitvec::Bits;
use hsmcore::hsm::types::{OwnedRange, RealmId, RecordId};
use hsmcore::merkle::{
    agent::Node, agent::StoreKey, agent::TreeStoreError, proof::ReadProof, Dir, HashOutput, KeyVec,
};

#[async_trait]
pub trait TreeStoreReader<HO: HashOutput>: Sync {
    async fn path_lookup(
        &self,
        realm_id: &RealmId,
        record_id: &RecordId,
    ) -> Result<HashMap<HO, Node<HO>>, TreeStoreError>;

    async fn read_node(
        &self,
        realm_id: &RealmId,
        key: StoreKey,
    ) -> Result<Node<HO>, TreeStoreError>;
}

pub async fn read<R: TreeStoreReader<HO>, HO: HashOutput>(
    realm_id: &RealmId,
    store: &R,
    range: &OwnedRange,
    root_hash: &HO,
    k: &RecordId,
) -> Result<ReadProof<HO>, TreeStoreError> {
    let mut nodes = store.path_lookup(realm_id, k).await?;
    let root = match nodes.remove(root_hash) {
        None => return Err(TreeStoreError::MissingNode),
        Some(Node::Leaf(_)) => panic!("found unexpected leaf node"),
        Some(Node::Interior(int)) => int,
    };
    let mut res = ReadProof::new(k.clone(), range.clone(), *root_hash, root);
    let full_key = KeyVec::from_record_id(k);
    let mut key = full_key.as_ref();
    loop {
        let n = res.path.last().unwrap();
        let d = Dir::from(key[0]);
        match n.branch(d) {
            None => return Ok(res),
            Some(b) => {
                if !key.starts_with(&b.prefix) {
                    return Ok(res);
                }
                key = key.slice_from(b.prefix.len());
                match nodes.remove(&b.hash) {
                    None => return Err(TreeStoreError::MissingNode),
                    Some(Node::Interior(int)) => {
                        res.path.push(int);
                        continue;
                    }
                    Some(Node::Leaf(v)) => {
                        assert!(key.is_empty());
                        res.leaf = Some(v);
                        return Ok(res);
                    }
                }
            }
        }
    }
}

// Reads down the tree from the root always following one side until a leaf is reached.
// Needed for merge.
pub async fn read_tree_side<R: TreeStoreReader<HO>, HO: HashOutput>(
    realm_id: &RealmId,
    store: &R,
    range: &OwnedRange,
    root_hash: &HO,
    side: Dir,
) -> Result<ReadProof<HO>, TreeStoreError> {
    let mut path = Vec::new();
    let mut key = KeyVec::new();
    let mut current = *root_hash;
    loop {
        match store
            .read_node(realm_id, StoreKey::new(&key, &current))
            .await?
        {
            Node::Interior(int) => match int.branch(side) {
                None => match int.branch(side.opposite()) {
                    None => {
                        path.push(int);
                        let k = if side == Dir::Right {
                            &range.end
                        } else {
                            &range.start
                        };
                        // TODO, should we remove key from ReadProof?
                        // this key is not a full key in this event.
                        // this can only happen for the root node.
                        return Ok(ReadProof {
                            key: k.clone(),
                            range: range.clone(),
                            root_hash: *root_hash,
                            leaf: None,
                            path,
                        });
                    }
                    Some(b) => {
                        current = b.hash;
                        key.extend(&b.prefix);
                        path.push(int);
                        continue;
                    }
                },
                Some(b) => {
                    current = b.hash;
                    key.extend(&b.prefix);
                    path.push(int);
                    continue;
                }
            },
            Node::Leaf(l) => {
                return Ok(ReadProof {
                    key: key.to_record_id(),
                    range: range.clone(),
                    root_hash: *root_hash,
                    leaf: Some(l),
                    path,
                });
            }
        }
    }
}
