// This file is part of HydraDX.

// Copyright (C) 2020-2022  Intergalactic, Limited (GIB).
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, Encode};
use cumulus_pallet_xcmp_queue::XcmDeferFilter;
use frame_support::dispatch::Weight;
use frame_support::traits::{Contains, EnsureOrigin};
use frame_support::{ensure, pallet_prelude::DispatchResult, traits::Get};
use frame_system::ensure_signed_or_root;
use frame_system::pallet_prelude::OriginFor;
use orml_traits::GetByKey;
use polkadot_core_primitives::BlockNumber;
use polkadot_parachain::primitives::RelayChainBlockNumber;
use scale_info::TypeInfo;
use sp_core::MaxEncodedLen;
use sp_runtime::traits::BlockNumberProvider;
use sp_runtime::traits::Convert;
use sp_runtime::traits::{AtLeast32BitUnsigned, CheckedAdd, CheckedDiv, CheckedMul, CheckedSub, Zero};
use sp_runtime::SaturatedConversion;
use sp_runtime::{ArithmeticError, DispatchError, RuntimeDebug, Saturating};
use xcm::lts::prelude::*;
use xcm::VersionedXcm;
use xcm::VersionedXcm::V3;
pub mod weights;

#[cfg(any(feature = "runtime-benchmarks", test))]
mod benchmarking;

#[cfg(test)]
mod tests;

// Re-export pallet items so that they can be accessed from the crate namespace.
pub use pallet::*;
pub use weights::WeightInfo;

#[derive(Clone, Default, Encode, Decode, RuntimeDebug, MaxEncodedLen, TypeInfo, Eq, PartialEq)]
pub struct AccumulatedDeferredAmount<BlockNumber> {
	pub amount: u128,
	pub last_updated: BlockNumber,
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use codec::HasCompact;
	use frame_support::pallet_prelude::*;
	use frame_support::traits::Contains;
	use polkadot_parachain::primitives::RelayChainBlockNumber;
	use sp_runtime::traits::BlockNumberProvider;
	use xcm::lts::MultiLocation;

	#[pallet::hooks]
	impl<T: Config> Hooks<T::BlockNumber> for Pallet<T> {}

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Identifier for the class of asset.
		type AssetId: Member
			+ Parameter
			+ Default
			+ Copy
			+ HasCompact
			+ MaybeSerializeDeserialize
			+ MaxEncodedLen
			+ TypeInfo
			+ AtLeast32BitUnsigned;

		type TechnicalOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// Defer duration base to be used for calculating the specific defer duration for any asset
		#[pallet::constant]
		type DeferDuration: Get<Self::BlockNumber>;

		// TODO: implement or remove
		#[pallet::constant]
		type MaxDeferDuration: Get<Self::BlockNumber>;

		type BlockNumberProvider: BlockNumberProvider<BlockNumber = Self::BlockNumber>;

		type CurrencyIdConvert: Convert<MultiLocation, Option<Self::AssetId>>;

		// TODO: Which type to use to define the rate limit here?
		type RateLimitFor: GetByKey<Self::AssetId, Option<u128>>;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;
	}

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	#[pallet::storage]
	/// Accumulated deferred amounts for each asset
	#[pallet::getter(fn accumulated_deferred_amount)]
	pub type AccumulatedDeferredAmounts<T: Config> =
		StorageMap<_, Blake2_128Concat, MultiLocation, AccumulatedDeferredAmount<T::BlockNumber>, ValueQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	pub enum Event<T: Config> {}

	#[pallet::error]
	#[cfg_attr(test, derive(PartialEq, Eq))]
	pub enum Error<T> {}

	#[pallet::call]
	impl<T: Config> Pallet<T> {}
}

fn get_loc_and_amount(m: &MultiAsset) -> Option<(MultiLocation, u128)> {
	match m.id {
		AssetId::Concrete(location) => match m.fun {
			Fungibility::Fungible(amount) => Some((location, amount)),
			_ => None,
		},
		_ => None,
	}
}

// TODO: pull out into hdx-math
pub fn calculate_deferred_duration(
	global_duration: BlockNumber,
	rate_limit: u128,
	incoming_amount: u128,
	accumulated_amount: u128,
	blocks_since_last_update: BlockNumber,
) -> BlockNumber {
	let total_accumulated = calculate_new_accumulated_amount(
		global_duration,
		rate_limit,
		incoming_amount,
		accumulated_amount,
		blocks_since_last_update,
	);
	let global_duration: u128 = global_duration.max(1).saturated_into();
	// duration * (incoming + decayed - rate_limit)
	let deferred_duration =
		global_duration.saturating_mul(total_accumulated.saturating_sub(rate_limit)) / rate_limit.max(1);

	deferred_duration.saturated_into()
}

pub fn calculate_new_accumulated_amount(
	global_duration: BlockNumber,
	rate_limit: u128,
	incoming_amount: u128,
	accumulated_amount: u128,
	blocks_since_last_update: BlockNumber,
) -> u128 {
	incoming_amount.saturating_add(decay(
		global_duration,
		rate_limit,
		accumulated_amount,
		blocks_since_last_update,
	))
}

pub fn decay(
	global_duration: BlockNumber,
	rate_limit: u128,
	accumulated_amount: u128,
	blocks_since_last_update: BlockNumber,
) -> u128 {
	let global_duration: u128 = global_duration.max(1).saturated_into();
	// acc - rate_limit * blocks / duration
	accumulated_amount
		.saturating_sub(rate_limit.saturating_mul(blocks_since_last_update.saturated_into()) / global_duration)
}

impl<T: Config> Pallet<T> {
	fn get_locations_and_amounts(instruction: &Instruction<T::RuntimeCall>) -> Vec<(MultiLocation, u128)> {
		use Instruction::*;
		match instruction {
			// NOTE: This does not address the native asset "coming back" from other chains.
			ReserveAssetDeposited(multi_assets) | ReceiveTeleportedAsset(multi_assets) => multi_assets
				.inner()
				.iter()
				.flat_map(|asset| get_loc_and_amount(asset))
				.collect(),
			_ => Vec::new(),
		}
	}
}

//TODO: what to do with para and snet at?
impl<T: Config> XcmDeferFilter<T::RuntimeCall> for Pallet<T> {
	fn deferred_by(
		para: polkadot_parachain::primitives::Id,
		sent_at: polkadot_core_primitives::BlockNumber,
		xcm: &VersionedXcm<T::RuntimeCall>,
	) -> Option<polkadot_core_primitives::BlockNumber> {
		if let V3(xcm) = xcm {
			if let Some(instruction) = xcm.first() {
				for (location, amount) in Pallet::<T>::get_locations_and_amounts(instruction) {
					let accumulated_liquidity = AccumulatedDeferredAmounts::<T>::get(location);

					let Some(asset_id) = T::CurrencyIdConvert::convert(location) else { continue };
					let Some(limit_per_duration) = T::RateLimitFor::get(&asset_id) else { continue };
					let defer_duration = T::DeferDuration::get();

					let current_time = T::BlockNumberProvider::current_block_number();
					let time_difference = current_time.saturating_sub(accumulated_liquidity.last_updated);

					let new_accumulated_amount = calculate_new_accumulated_amount(
						defer_duration.saturated_into(),
						limit_per_duration,
						amount,
						accumulated_liquidity.amount,
						time_difference.saturated_into(),
					);
					// TODO: avoid redoing computation?
					let deferred_by = calculate_deferred_duration(
						defer_duration.saturated_into(),
						limit_per_duration,
						amount,
						accumulated_liquidity.amount,
						time_difference.saturated_into(),
					);

					AccumulatedDeferredAmounts::<T>::insert(
						location,
						AccumulatedDeferredAmount {
							amount: new_accumulated_amount,
							last_updated: current_time,
						},
					);

					if deferred_by > 0 {
						return Some(deferred_by);
					} else {
						return None;
					}
				}
			}
		}

		None
	}
}