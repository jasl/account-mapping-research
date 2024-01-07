// This file is part of EVM Account Mapping Pallet.

// Copyright (C) HashForest Technology Pte. Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub use pallet::*;

mod eip712;
mod encode;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
pub mod weights;
pub use weights::WeightInfo;

/// The log target of this pallet.
pub const LOG_TARGET: &str = "runtime::evm_account_mapping";

// Syntactic sugar for logging.
#[macro_export]
macro_rules! log {
	($level:tt, $patter:expr $(, $values:expr)* $(,)?) => {
		log::$level!(
			target: $crate::LOG_TARGET,
			concat!("[{:?}] ", $patter), <frame_system::Pallet<T>>::block_number() $(, $values)*
		)
	};
}

use alloc::{boxed::Box, vec::Vec};
use codec::{Decode, Encode};
use scale_info::TypeInfo;
use frame_support::{
	dispatch::{DispatchInfo, GetDispatchInfo, PostDispatchInfo, RawOrigin},
	traits::{
		tokens::{Fortitude, Preservation},
		fungible::Inspect as InspectFungible,
		Contains, Imbalance, OriginTrait,
		Currency, OnUnbalanced
	},
	weights::Weight,
	Parameter,
};
use pallet_transaction_payment::OnChargeTransaction;
use sp_core::crypto::AccountId32;
use sp_io::hashing::blake2_256;
use sp_runtime::{
	traits::{IdentifyAccount, Dispatchable, Verify, SaturatedConversion, Hash},
	FixedPointOperand, RuntimeDebug,
};

type PaymentOnChargeTransaction<T> = <T as pallet_transaction_payment::Config>::OnChargeTransaction;

type PaymentBalanceOf<T> = <<T as pallet_transaction_payment::Config>::OnChargeTransaction as OnChargeTransaction<T>>::Balance;

type BalanceOf<T> = <<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;
type NegativeImbalanceOf<T> = <<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::NegativeImbalance;
pub type EIP712ChainID = sp_core::U256;
pub type EIP712VerifyingContractAddress = sp_core::H160;
pub type EIP712Signature = [u8; 65];

pub type Nonce = u64;
pub type AccountId32Bytes = [u8; 32];
pub type Keccak256Signature = [u8; 32];

pub enum Secp256K1PublicKeyForm {
	Compressed,
	Uncompressed,
}

pub trait AddressConversion<AccountId>: Sized {
	const SECP256K1_PUBLIC_KEY_FORM: Secp256K1PublicKeyForm;

	fn try_convert(evm_public_key: &[u8]) -> Option<AccountId>;
}

pub struct SubstrateAddressConverter;
impl AddressConversion<AccountId32> for SubstrateAddressConverter {
	const SECP256K1_PUBLIC_KEY_FORM: Secp256K1PublicKeyForm = Secp256K1PublicKeyForm::Compressed;

	fn try_convert(evm_public_key: &[u8]) -> Option<AccountId32> {
		AccountId32::decode(&mut &blake2_256(evm_public_key)[..]).ok()
	}
}

pub struct EvmTransparentConverter;
impl AddressConversion<AccountId32> for EvmTransparentConverter {
	const SECP256K1_PUBLIC_KEY_FORM: Secp256K1PublicKeyForm = Secp256K1PublicKeyForm::Uncompressed;

	fn try_convert(evm_public_key: &[u8]) -> Option<AccountId32> {
		let h32 = sp_core::H256(sp_io::hashing::keccak_256(evm_public_key));
		let h20 = sp_core::H160::from(h32);
		let postfix = b"@evm_address";

		let mut raw_account: AccountId32Bytes = [0; 32];
		raw_account[..20].copy_from_slice(h20.as_bytes());
		raw_account[20..].copy_from_slice(postfix);

		Some(AccountId32::from(raw_account))
	}
}

#[derive(Clone, Eq, PartialEq, Encode, Decode, RuntimeDebug, TypeInfo)]
pub struct Cheque<BlockNumber, Balance, Hash, AccountId> {
	/// The cheque only available before or at the block number.
	pub deadline: BlockNumber,
	/// The cheque only available when the sponsor's balance greater or equal than the value.
	pub sponsor_minimum_balance: Balance,
	/// Restrict the cheque to a particular account.
	pub only_account: Option<AccountId>,
	/// Restrict the caller's hash must equal the value.
	pub only_account_nonce: Option<Nonce>,
	/// Restrict the cheque to a particular call.
	pub only_call_hash: Option<Hash>,
	/// Sponsor max tip amount, set 0 if you don't want to sponsor any tip
	pub sponsor_maximum_tip: Balance,
}

#[derive(Clone, Eq, PartialEq, Encode, Decode, RuntimeDebug, TypeInfo)]
pub struct PreSignedCheque<BlockNumber, Balance, Hash, AccountId, Signature> {
	pub cheque: Cheque<BlockNumber, Balance, Hash, AccountId>,
	pub signature: Signature,
	pub signer: AccountId,
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	/// Configure the pallet by specifying the parameters and types on which it depends.
	#[pallet::config]
	pub trait Config: frame_system::Config + pallet_transaction_payment::Config {
		/// Because this pallet emits events, it depends on the runtime's definition of an event.
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The overarching call type.
		type RuntimeCall: Parameter
			+ Dispatchable<
				RuntimeOrigin = Self::RuntimeOrigin,
				Info = DispatchInfo,
				PostInfo = PostDispatchInfo,
			> + GetDispatchInfo
			+ codec::Decode
			+ codec::Encode
			+ scale_info::TypeInfo
			+ IsType<<Self as frame_system::Config>::RuntimeCall>;

		/// The system's currency for payment.
		type Currency: InspectFungible<Self::AccountId> + Currency<Self::AccountId>;

		type AddressConverter: AddressConversion<Self::AccountId>;

		#[pallet::constant]
		type ServiceFee: Get<BalanceOf<Self>>;

		type OnUnbalancedForServiceFee: OnUnbalanced<NegativeImbalanceOf<Self>>;

		type CallFilter: Contains<<Self as frame_system::Config>::RuntimeCall>;

		type SponsoredCallFilter: Contains<<Self as frame_system::Config>::RuntimeCall>;

		#[pallet::constant]
		type EIP712Name: Get<Vec<u8>>;

		#[pallet::constant]
		type EIP712Version: Get<Vec<u8>>;

		#[pallet::constant]
		type EIP712ChainID: Get<EIP712ChainID>;

		#[pallet::constant]
		type EIP712VerifyingContractAddress: Get<EIP712VerifyingContractAddress>;

		/// Off-Chain signature type.
		///
		/// Can verify whether an `Self::OffchainPublic` created a signature.
		type OffchainSignature: Verify<Signer = Self::OffchainPublic> + Parameter;

		/// Off-Chain public key.
		///
		/// Must identify as an on-chain `Self::AccountId`.
		type OffchainPublic: IdentifyAccount<AccountId = Self::AccountId>;

		/// Type representing the weight of this pallet
		type WeightInfo: WeightInfo;
	}

	// Pallets use events to inform users when important changes are made.
	// https://docs.substrate.io/main-docs/build/events-errors/
	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		ServiceFeePaid {
			who: T::AccountId,
			actual_fee: BalanceOf<T>,
			expected_fee: BalanceOf<T>,
		},
		TransactionFeePaid {
			who: T::AccountId,
			actual_fee: PaymentBalanceOf<T>,
			tip: PaymentBalanceOf<T>,
		},
		CallDone {
			who: T::AccountId,
			call_result: DispatchResultWithPostInfo,
		},
	}

	// Errors inform users that something went wrong.
	#[pallet::error]
	pub enum Error<T> {
		Unexpected,
		NonceError,
		PaymentError,
	}

	#[pallet::storage]
	pub(crate) type AccountNonce<T: Config> =
		StorageMap<_, Blake2_128Concat, T::AccountId, u64, ValueQuery>;

	#[pallet::validate_unsigned]
	impl<T: Config> ValidateUnsigned for Pallet<T>
	where
		PaymentBalanceOf<T>: FixedPointOperand,
		BalanceOf<T>: FixedPointOperand,
		<T as frame_system::Config>::RuntimeCall:
			Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
		<T as frame_system::Config>::AccountId: From<AccountId32Bytes> + Into<AccountId32Bytes>,
		T: frame_system::Config<AccountId = AccountId32>,
	{
		type Call = Call<T>;

		fn validate_unsigned(
			_source: TransactionSource,
			unsigned_call: &Self::Call,
		) -> TransactionValidity {
			// Only allow `meta_call`
			let Call::meta_call {
				ref who,
				ref call,
				ref nonce,
				ref tip,
				ref pre_signed_cheque,
				ref signature,
			} = unsigned_call else {
				return Err(InvalidTransaction::Call.into())
			};

			// Check the signature and get the public key
			let call_data = <T as Config>::RuntimeCall::encode(call);
			let message_hash = Self::eip712_message_hash(&who, &call_data, nonce, tip, pre_signed_cheque);

			let Ok(recovered_public_key) = (match <T as Config>::AddressConverter::SECP256K1_PUBLIC_KEY_FORM {
				Secp256K1PublicKeyForm::Compressed => {
					sp_io::crypto::secp256k1_ecdsa_recover_compressed(signature, &message_hash)
						.map(|i| i.to_vec())
				},
				Secp256K1PublicKeyForm::Uncompressed => {
					sp_io::crypto::secp256k1_ecdsa_recover(signature, &message_hash)
						.map(|i| i.to_vec())
				}
			}) else {
				return Err(InvalidTransaction::Call.into())
			};

			// Deserialize the actual caller
			let Some(decoded_account) =
				<T as Config>::AddressConverter::try_convert(&recovered_public_key) else {
				return Err(InvalidTransaction::Call.into())
			};
			if who != &decoded_account {
				return Err(InvalidTransaction::BadSigner.into())
			}

			// Skip frame_system::CheckNonZeroSender
			// Skip frame_system::CheckSpecVersion<Runtime>
			// Skip frame_system::CheckTxVersion<Runtime>
			// Skip frame_system::CheckGenesis<Runtime>
			// Skip frame_system::CheckEra<Runtime>

			// frame_system::CheckNonce<Runtime>
			let account_nonce = AccountNonce::<T>::get(&who);
			if nonce < &account_nonce {
				return Err(InvalidTransaction::Stale.into())
			}
			let provides = (who.clone(), nonce).encode();
			let requires = if &account_nonce < nonce && nonce > &0u64 {
				Some((who.clone(), nonce - 1).encode())
			} else {
				None
			};
			if nonce != &account_nonce {
				return Err(if nonce < &account_nonce {
					InvalidTransaction::Stale
				} else {
					InvalidTransaction::Future
				}
				.into())
			}

			// Skip frame_system::CheckWeight<Runtime>
			// it has implemented `validate_unsigned` and `pre_dispatch_unsigned`, we don't need to
			// do the validate here.

			// pallet_transaction_payment::ChargeTransactionPayment<Runtime>
			let tip = tip.unwrap_or(0u32.into());
			let len = call.encoded_size();
			let info = call.get_dispatch_info();
			// We shall get the same `fee` later
			let est_fee =
				pallet_transaction_payment::Pallet::<T>::compute_fee(len as u32, &info, tip);
			// TODO: Need check this work with assets-payment
			// We don't withdraw the fee here, because we can't cache the imbalance
			// Instead, we check the account has enough fee
			// I think this is a hack, or the type can't match
			let est_fee = est_fee.saturated_into::<u128>();
			// We can't get the actual size of the meta-tx itself,
			// so we have to introducing service fee.
			let service_fee = T::ServiceFee::get().saturated_into::<u128>();

			if let Some(
				PreSignedCheque {
					cheque,
					signature,
					signer,
				}
			) = pre_signed_cheque {
				let encoded_cheque = Encode::encode(&cheque);
				if !signature.verify(&*encoded_cheque, signer) {
					// NOTE: for security reasons modern UIs implicitly wrap the data requested to sign into
					// <Bytes></Bytes>, that's why we support both wrapped and raw versions.
					let prefix = b"<Bytes>";
					let suffix = b"</Bytes>";
					let mut wrapped: Vec<u8> = Vec::with_capacity(encoded_cheque.len() + prefix.len() + suffix.len());
					wrapped.extend(prefix);
					wrapped.extend(&encoded_cheque);
					wrapped.extend(suffix);

					if !signature.verify(&*wrapped, signer) {
						return Err(InvalidTransaction::Payment.into())
					}
				}

				if let Some(only_account) = &cheque.only_account {
					if only_account != who {
						return Err(InvalidTransaction::Payment.into())
					}
				}
				if let Some(only_account_nonce) = &cheque.only_account_nonce {
					if only_account_nonce != &account_nonce {
						return Err(InvalidTransaction::Payment.into())
					}
				}

				if let Some(only_call_hash) = cheque.only_call_hash {
					if T::Hashing::hash(&call_data) != only_call_hash {
						return Err(InvalidTransaction::Payment.into())
					}
				}

				let now = frame_system::Pallet::<T>::block_number();
				if cheque.deadline < now {
					return Err(InvalidTransaction::Payment.into())
				};

				let usable_balance_for_fees =
					T::Currency::reducible_balance(&signer, Preservation::Preserve, Fortitude::Polite)
						.saturated_into::<u128>();
				if est_fee.saturating_add(service_fee) > usable_balance_for_fees {
					return Err(InvalidTransaction::Payment.into())
				}
				if cheque.sponsor_minimum_balance.saturated_into() > usable_balance_for_fees {
					return Err(InvalidTransaction::Payment.into())
				}
				if cheque.sponsor_maximum_tip < tip {
					return Err(InvalidTransaction::Payment.into())
				}
			} else {
				let usable_balance_for_fees =
					T::Currency::reducible_balance(&who, Preservation::Preserve, Fortitude::Polite)
						.saturated_into::<u128>();
				if est_fee.saturating_add(service_fee) > usable_balance_for_fees {
					return Err(InvalidTransaction::Payment.into())
				}
			}

			// Calculate priority
			// Cheat from `get_priority` in frame/transaction-payment/src/lib.rs
			use frame_support::traits::Defensive;
			use sp_runtime::{traits::One, SaturatedConversion, Saturating};
			// Calculate how many such extrinsics we could fit into an empty block and take the
			// limiting factor.
			let max_block_weight = <T as frame_system::Config>::BlockWeights::get().max_block;
			let max_block_length =
				*<T as frame_system::Config>::BlockLength::get().max.get(info.class) as u64;

			// bounded_weight is used as a divisor later so we keep it non-zero.
			let bounded_weight = info.weight.max(Weight::from_parts(1, 1)).min(max_block_weight);
			let bounded_length = (len as u64).clamp(1, max_block_length);

			// returns the scarce resource, i.e. the one that is limiting the number of
			// transactions.
			let max_tx_per_block_weight = max_block_weight
				.checked_div_per_component(&bounded_weight)
				.defensive_proof("bounded_weight is non-zero; qed")
				.unwrap_or(1);
			let max_tx_per_block_length = max_block_length / bounded_length;
			// Given our current knowledge this value is going to be in a reasonable range - i.e.
			// less than 10^9 (2^30), so multiplying by the `tip` value is unlikely to overflow the
			// balance type. We still use saturating ops obviously, but the point is to end up with
			// some `priority` distribution instead of having all transactions saturate the
			// priority.
			let max_tx_per_block = max_tx_per_block_length
				.min(max_tx_per_block_weight)
				.saturated_into::<PaymentBalanceOf<T>>();
			let max_reward = |val: PaymentBalanceOf<T>| val.saturating_mul(max_tx_per_block);

			// To distribute no-tip transactions a little bit, we increase the tip value by one.
			// This means that given two transactions without a tip, smaller one will be preferred.
			let tip = tip.saturating_add(One::one());
			let scaled_tip = max_reward(tip);

			let priority = scaled_tip.saturated_into::<TransactionPriority>();

			// Finish the validation
			let valid_transaction_builder = ValidTransaction::with_tag_prefix("EVMAccountMapping")
				.priority(priority)
				.and_provides(provides)
				.longevity(5)
				.propagate(true);
			let Some(requires) = requires else { return valid_transaction_builder.build() };
			valid_transaction_builder.and_requires(requires).build()
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T>
	where
		PaymentBalanceOf<T>: FixedPointOperand,
		BalanceOf<T>: FixedPointOperand,
		<T as frame_system::Config>::RuntimeCall:
			Dispatchable<Info = DispatchInfo, PostInfo = PostDispatchInfo>,
		T: frame_system::Config<AccountId = AccountId32>,
	{
		/// Meta-transaction from EVM compatible chains
		#[pallet::call_index(0)]
		#[pallet::weight({
			let di = call.get_dispatch_info();
			(
				T::WeightInfo::meta_call().saturating_add(di.weight),
				di.class
			)
		})]
		pub fn meta_call(
			origin: OriginFor<T>,
			who: T::AccountId,
			call: Box<<T as Config>::RuntimeCall>,
			nonce: Nonce,
			tip: Option<PaymentBalanceOf<T>>,
			pre_signed_cheque: Option<PreSignedCheque<BlockNumberFor<T>, PaymentBalanceOf<T>, T::Hash, T::AccountId, T::OffchainSignature>>,
			#[allow(unused_variables)] signature: EIP712Signature,
		) -> DispatchResult {
			// This is an unsigned transaction
			ensure_none(origin)?;

			// We don't need to re-validate the `signature` here,
			// because it already validated in `validate_unsigned` stage,
			// and it should no way to skip.
			// TODO: Confirm this.

			// We don't need to re-validate the `pre_signed_cheque` here,
			// because it already validated in `validate_unsigned` stage,
			// and it should no way to skip.
			// TODO: Confirm this.
			let payer = if let Some(PreSignedCheque {ref signer, .. }) = pre_signed_cheque {
				signer
			} else {
				&who
			};

			// It is possible that an account passed `validate_unsigned` check,
			// but for some reason, its balance isn't enough for the service fee.
			use frame_support::traits::tokens::{WithdrawReasons, ExistenceRequirement};
			// NOTE: it is possible that the account doesn't have enough fee, which is a vulnerable.
			let withdrawn = T::Currency::withdraw(
				payer,
				T::ServiceFee::get(),
				WithdrawReasons::FEE,
				ExistenceRequirement::KeepAlive
			).map_err(|_err| Error::<T>::PaymentError)?;
			let withdrawn_fee = withdrawn.peek();
			T::OnUnbalancedForServiceFee::on_unbalanced(withdrawn);
			Self::deposit_event(Event::ServiceFeePaid {
				who: payer.clone(),
				actual_fee: withdrawn_fee,
				expected_fee: T::ServiceFee::get(),
			});

			// Bump the nonce
			AccountNonce::<T>::try_mutate(&who, |value| {
				if *value != nonce {
					return Err(Error::<T>::NonceError)
				}
				*value += 1;
				Ok(())
			})?;

			// Call
			let mut origin: T::RuntimeOrigin = RawOrigin::Signed(who.clone()).into();
			if let Some(PreSignedCheque { ref cheque, .. }) = pre_signed_cheque {
				if cheque.only_call_hash.is_none() {
					origin.add_filter(T::SponsoredCallFilter::contains);
				}
			} else {
				origin.add_filter(T::CallFilter::contains);
			}
			let len = call.encoded_size();
			let info = call.get_dispatch_info();
			let tip = tip.unwrap_or(0u32.into());
			let est_fee =
				pallet_transaction_payment::Pallet::<T>::compute_fee(len as u32, &info, tip);
			// Add the service fee
			let already_withdrawn =
				<PaymentOnChargeTransaction<T> as OnChargeTransaction<T>>::withdraw_fee(
					payer,
					&(*call).clone().into(),
					&info,
					est_fee,
					tip,
				)
				.map_err(|_err| Error::<T>::PaymentError)?;

			let call_result = call.dispatch(origin);
			let post_info =
				call_result.unwrap_or_else(|error_and_info| error_and_info.post_info);
			// Deposit the call's result
			Self::deposit_event(Event::CallDone { who: who.clone(), call_result });

			let actual_fee = pallet_transaction_payment::Pallet::<T>::compute_actual_fee(
				len as u32, &info, &post_info, tip,
			);
			// frame/transaction-payment/src/payment.rs
			<PaymentOnChargeTransaction<T> as OnChargeTransaction<T>>::correct_and_deposit_fee(
				payer,
				&info,
				&post_info,
				actual_fee,
				tip,
				already_withdrawn,
			).map_err(|_err| Error::<T>::PaymentError)?;
			Self::deposit_event(Event::TransactionFeePaid { who: payer.clone(), actual_fee, tip });

			Ok(())
		}
	}

	impl<T: Config> Pallet<T>
	where
		T: frame_system::Config<AccountId = AccountId32>,
	{
		pub(crate) fn eip712_message_hash(
			who: &T::AccountId,
			call_data: &[u8],
			nonce: &Nonce,
			tip: &Option<PaymentBalanceOf<T>>,
			pre_signed_cheque: &Option<PreSignedCheque<BlockNumberFor<T>, PaymentBalanceOf<T>, T::Hash, T::AccountId, T::OffchainSignature>>,
		) -> Keccak256Signature {
			use alloc::vec;

			// TODO: will refactor this in Kevin's way for performance.
			let eip712_domain = crate::eip712::EIP712Domain {
				name: T::EIP712Name::get(),
				version: T::EIP712Version::get(),
				chain_id: T::EIP712ChainID::get(),
				verifying_contract: T::EIP712VerifyingContractAddress::get(),
				salt: None,
			};
			let domain_separator = eip712_domain.separator();

			let type_hash = sp_io::hashing::keccak_256(
				"SubstrateCall(string who,bytes callData,uint64 nonce,uint128 tip,bytes preSignedCheque)".as_bytes(),
			);
			use sp_core::crypto::Ss58Codec;
			let ss58_who = who.to_ss58check_with_version(T::SS58Prefix::get().into());
			let mut message_tokens = vec![
				ethabi::Token::FixedBytes(type_hash.to_vec()),
				ethabi::Token::FixedBytes(sp_io::hashing::keccak_256(ss58_who.as_bytes()).to_vec()),
				ethabi::Token::FixedBytes(sp_io::hashing::keccak_256(call_data).to_vec()),
				ethabi::Token::Uint((*nonce).into()),
			];
			if let Some(tip) = tip {
				let tip = (*tip).saturated_into::<u128>();
				message_tokens.push(
					ethabi::Token::Uint(tip.into())
				);
			}
			if let Some(pre_signed_cheque) = pre_signed_cheque {
				message_tokens.push(
					ethabi::Token::FixedBytes(sp_io::hashing::keccak_256(&Encode::encode(pre_signed_cheque)).to_vec())
				);
			}

			let message_hash = sp_io::hashing::keccak_256(&ethabi::encode(&message_tokens));
			let typed_data_hash_input = &vec![
				crate::encode::SolidityDataType::String("\x19\x01"),
				crate::encode::SolidityDataType::Bytes(&domain_separator),
				crate::encode::SolidityDataType::Bytes(&message_hash),
			];
			let bytes = crate::encode::abi::encode_packed(typed_data_hash_input);
			sp_io::hashing::keccak_256(bytes.as_slice())
		}
	}
}
