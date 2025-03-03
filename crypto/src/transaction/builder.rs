use std::ops::Deref;

use ark_ff::{UniformRand, Zero};
use rand::seq::SliceRandom;
use rand_core::{CryptoRng, RngCore};

use super::Error;
use crate::{
    action::{output, spend, Action},
    asset, ka,
    keys::{OutgoingViewingKey, SpendKey},
    memo::MemoPlaintext,
    merkle,
    rdsa::{Binding, Signature, SigningKey, SpendAuth},
    transaction::{Fee, Transaction, TransactionBody},
    value, Address, Fr, Note, Output, Spend, Value,
};

/// Used to construct a Penumbra transaction.
pub struct Builder {
    /// List of spends. We store the spend key and body rather than a Spend
    /// so we can defer signing until the complete transaction is ready.
    pub spends: Vec<(SigningKey<SpendAuth>, spend::Body)>,
    /// List of outputs in the transaction.
    pub outputs: Vec<Output>,
    /// Transaction fee. None if unset.
    pub fee: Option<Fee>,
    /// Sum of blinding factors for each value commitment.
    pub synthetic_blinding_factor: Fr,
    /// Sum of value commitments.
    pub value_commitments: decaf377::Element,
    /// Value balance.
    pub value_balance: decaf377::Element,
    /// The root of the note commitment merkle tree.
    pub merkle_root: merkle::Root,
    /// Expiry height. None if unset.
    pub expiry_height: Option<u32>,
    /// Chain ID. None if unset.
    pub chain_id: Option<String>,
}

impl Builder {
    /// Create a new `Spend` to spend an existing note.
    pub fn add_spend<R: RngCore + CryptoRng>(
        mut self,
        rng: &mut R,
        spend_key: SpendKey,
        merkle_path: merkle::Path,
        note: Note,
        position: merkle::Position,
    ) -> Self {
        let v_blinding = Fr::rand(rng);
        let value_commitment = note.value().commit(v_blinding);
        // We add to the transaction's value balance.
        self.synthetic_blinding_factor += v_blinding;
        self.value_balance +=
            Fr::from(note.value().amount) * note.value().asset_id.value_generator();

        let spend_auth_randomizer = Fr::rand(rng);
        let rsk = spend_key.spend_auth_key().randomize(&spend_auth_randomizer);

        let body = spend::Body::new(
            rng,
            value_commitment,
            *spend_key.spend_auth_key(),
            spend_auth_randomizer,
            merkle_path,
            position,
            note,
            v_blinding,
            *spend_key.nullifier_key(),
        );
        self.value_commitments += value_commitment.0;

        self.spends.push((rsk, body));

        self
    }

    /// Generate a new note and add it to the output, returning a clone of the generated note.
    ///
    /// For chaining output, use [`Builder::add_output`].
    pub fn add_output_producing_note<R: RngCore + CryptoRng>(
        mut self,
        rng: &mut R,
        dest: &Address,
        value_to_send: Value,
        memo: MemoPlaintext,
        ovk: &OutgoingViewingKey,
    ) -> (Note, Self) {
        let note = Note::generate(rng, dest, value_to_send);
        let diversified_generator = note.diversified_generator();
        let transmission_key = note.transmission_key();
        let value_to_send = note.value();

        let v_blinding = Fr::rand(rng);

        let esk = ka::Secret::new(rng);
        let encrypted_memo = memo.encrypt(&esk, dest);

        // We subtract from the transaction's value balance.
        self.synthetic_blinding_factor -= v_blinding;
        self.value_balance -=
            Fr::from(value_to_send.amount) * value_to_send.asset_id.value_generator();

        let body = output::Body::new(
            note.clone(),
            v_blinding,
            diversified_generator,
            transmission_key,
            &esk,
        );
        self.value_commitments -= body.value_commitment.0;

        let ovk_wrapped_key = note.encrypt_key(&esk, ovk, body.value_commitment);

        self.outputs.push(Output {
            body,
            encrypted_memo,
            ovk_wrapped_key,
        });

        (note, self)
    }

    /// Create a new `Output`, implicitly creating a new note for it and encrypting the provided
    /// [`MemoPlaintext`] with a fresh ephemeral secret key.
    ///
    /// To return the generated note, use [`Builder::add_output_producing_note`].
    pub fn add_output<R: RngCore + CryptoRng>(
        self,
        rng: &mut R,
        dest: &Address,
        value_to_send: Value,
        memo: MemoPlaintext,
        ovk: &OutgoingViewingKey,
    ) -> Self {
        self.add_output_producing_note(rng, dest, value_to_send, memo, ovk)
            .1
    }

    /// Set the transaction fee in PEN.
    ///
    /// Note that we're using the lower case `pen` in the code.
    pub fn set_fee(mut self, fee: u64) -> Self {
        let asset_id = asset::REGISTRY.parse_denom("upenumbra").unwrap().id();
        let fee_value = Value {
            amount: fee,
            asset_id: asset_id.clone(),
        };

        let fee_v_blinding = Fr::zero();
        let value_commitment = fee_value.commit(fee_v_blinding);

        // The fee is effectively an additional output, so we
        // add to the transaction's value balance.
        self.synthetic_blinding_factor -= fee_v_blinding;
        self.value_balance -= Fr::from(fee) * asset_id.value_generator();
        self.value_commitments -= value_commitment.0;

        self.fee = Some(Fee(fee));
        self
    }

    /// Set the expiry height.
    pub fn set_expiry_height(mut self, expiry_height: u32) -> Self {
        self.expiry_height = Some(expiry_height);
        self
    }

    /// Set the chain ID.
    pub fn set_chain_id(mut self, chain_id: String) -> Self {
        self.chain_id = Some(chain_id);
        self
    }

    /// Add the binding signature based on the current sum of synthetic blinding factors.
    #[allow(non_snake_case)]
    pub fn compute_binding_sig<R: CryptoRng + RngCore>(
        &self,
        rng: &mut R,
        sighash: &[u8; 64],
    ) -> Signature<Binding> {
        let binding_signing_key: SigningKey<Binding> = self.synthetic_blinding_factor.into();

        // Check that the derived verification key corresponds to the signing key to be used.
        let H = value::VALUE_BLINDING_GENERATOR.deref();
        let binding_verification_key_raw = (self.synthetic_blinding_factor * H).compress().0;

        // If value balance is non-zero, the verification key would be value_commitments - value_balance,
        // but value_balance should always be zero.
        let computed_verification_key = self.value_commitments.compress().0;
        assert_eq!(binding_verification_key_raw, computed_verification_key);

        binding_signing_key.sign(rng, sighash)
    }

    pub fn finalize<R: CryptoRng + RngCore>(
        mut self,
        mut rng: &mut R,
    ) -> Result<Transaction, Error> {
        if self.chain_id.is_none() {
            return Err(Error::NoChainID);
        }

        if self.fee.is_none() {
            return Err(Error::FeeNotSet);
        }

        if self.value_balance != decaf377::Element::default() {
            return Err(Error::NonZeroValueBalance);
        }

        let mut actions = Vec::<Action>::new();

        // Randomize all actions to minimize info leakage.
        self.spends.shuffle(rng);
        self.outputs.shuffle(rng);

        // Fill in the spends using blank signatures, so we can build the sighash tx
        for (_, body) in &self.spends {
            actions.push(Action::Spend(Spend {
                body: body.clone(),
                auth_sig: Signature::from([0; 64]),
            }));
        }
        for output in self.outputs.drain(..) {
            actions.push(Action::Output(output));
        }

        let mut transaction_body = TransactionBody {
            actions,
            merkle_root: self.merkle_root.clone(),
            expiry_height: self.expiry_height.unwrap_or(0),
            chain_id: self.chain_id.take().unwrap(),
            fee: self.fee.take().unwrap(),
        };

        // The transaction body is filled except for the signatures,
        // so we can compute the sighash value....
        let sighash = transaction_body.sighash();

        // and use it to fill in the spendauth sigs...
        for i in 0..self.spends.len() {
            let (rsk, _) = self.spends[i];
            if let Action::Spend(Spend {
                ref mut auth_sig, ..
            }) = transaction_body.actions[i]
            {
                *auth_sig = rsk.sign(&mut rng, &sighash);
            } else {
                unreachable!("spends come first in actions list")
            }
        }

        // ... and the binding sig
        let binding_sig = self.compute_binding_sig(rng, &sighash);

        Ok(Transaction {
            transaction_body,
            binding_sig,
        })
    }
}
