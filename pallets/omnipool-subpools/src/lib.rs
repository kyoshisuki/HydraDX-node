// Copyright (C) 2020-2023  Intergalactic, Limited (GIB).
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

//! # Omnipool-subpools pallet
//!
//! Omnipool subpool support implementation
//!
//!
//! TDB

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::too_many_arguments)]

mod benchmarks;
#[cfg(test)]
mod tests;
mod types;
pub mod weights;

use crate::types::{AssetDetail, Balance};
use frame_support::pallet_prelude::*;
use frame_support::require_transactional;
use hydra_dx_math::omnipool_subpools::MigrationDetails;
use hydra_dx_math::support::traits::{CheckedDivInner, CheckedMulInner, CheckedMulInto, Convert};
use orml_traits::currency::MultiCurrency;
use sp_std::prelude::*;

use hydra_dx_math::omnipool::types::I129;
use hydra_dx_math::omnipool::*;
use hydra_dx_math::stableswap::MAX_D_ITERATIONS;
use hydra_dx_math::stableswap::*;

pub use pallet::*;
use pallet_omnipool::types::{Position, Tradability};

type OmnipoolPallet<T> = pallet_omnipool::Pallet<T>;
type StableswapPallet<T> = pallet_stableswap::Pallet<T>;

type AssetIdOf<T> = <T as pallet_omnipool::Config>::AssetId;
type StableswapAssetIdOf<T> = <T as pallet_stableswap::Config>::AssetId;
type CurrencyOf<T> = <T as pallet_omnipool::Config>::Currency;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use crate::weights::WeightInfo;
	use frame_system::pallet_prelude::*;
	use hydra_dx_math::omnipool::types::{AssetStateChange, BalanceUpdate};
	use pallet_omnipool::types::{AssetState, Tradability};
	use pallet_stableswap::types::AssetLiquidity;
	use sp_runtime::traits::Zero;
	use sp_runtime::{ArithmeticError, Permill, Rational128};

	#[pallet::pallet]
	#[pallet::generate_store(pub (crate) trait Store)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config + pallet_omnipool::Config + pallet_stableswap::Config {
		/// The overarching event type.
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// Checks that an origin has the authority to manage a subpool.
		type AuthorityOrigin: EnsureOrigin<Self::Origin>;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;
	}

	#[pallet::storage]
	#[pallet::getter(fn migrated_assets)]
	/// Details of asset migrated from Omnipool to a subpool.
	/// Key is id of migrated asset.
	/// Value is tuple of (Subpool id, AssetDetail).
	pub(super) type MigratedAssets<T: Config> =
		StorageMap<_, Blake2_128Concat, AssetIdOf<T>, (StableswapAssetIdOf<T>, AssetDetail), OptionQuery>;

	#[pallet::storage]
	#[pallet::getter(fn subpools)]
	/// Existing subpool IDs.
	pub(super) type Subpools<T: Config> = StorageMap<_, Blake2_128Concat, StableswapAssetIdOf<T>, (), OptionQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub (crate) fn deposit_event)]
	pub enum Event<T: Config> {
		SubpoolCreated {
			id: StableswapAssetIdOf<T>,
			assets: (AssetIdOf<T>, AssetIdOf<T>),
		},
		AssetMigrated {
			asset_id: AssetIdOf<T>,
			pool_id: StableswapAssetIdOf<T>,
		},
	}

	#[pallet::error]
	#[cfg_attr(test, derive(PartialEq, Eq))]
	pub enum Error<T> {
		/// Stableswap subpool does not exist.
		SubpoolNotFound,
		/// Asset ID of stable asset is not specified.
		WithdrawAssetNotSpecified,
		/// Given asset id is not stable asset.
		NotStableAsset,
		/// Overflow
		Math,
		/// Trade limit exceeded.
		LimitExceeded,
		/// Trade limit not reached.
		LimitNotReached,
		/// Not allowed to perform an operation on given asset.
		NotAllowed,
	}

	#[pallet::call]
	impl<T: Config> Pallet<T>
	where
		<T as pallet_omnipool::Config>::AssetId:
			Into<<T as pallet_stableswap::Config>::AssetId> + From<<T as pallet_stableswap::Config>::AssetId>,
	{
		/// Create new subpool by migrating 2 assets from Omnipool to new Stableswap subpool.
		///
		/// New subpool can only be created from precisely 2 assets.
		///
		/// Subpool ID (share asset id) must be pre-registered.
		///
		/// Subpool creation steps:
		/// - create stableswap pool
		/// - set tradable state of each asset to preserve the same state as previously in the Omnipool
		/// - move liquidity from Omnipool account to subpool account
		/// - remove both assets from Omnipool
		/// - add share asset as new asset in Omnipool
		/// - save both asset's details in subpool storage
		///
		/// Emits `SubpoolCreated` event when successful
		#[pallet::call_index(0)]
		#[pallet::weight(<T as Config>::WeightInfo::create_subpool())]
		pub fn create_subpool(
			origin: OriginFor<T>,
			share_asset: AssetIdOf<T>,
			asset_a: AssetIdOf<T>,
			asset_b: AssetIdOf<T>,
			share_asset_weight_cap: Permill,
			amplification: u16,
			trade_fee: Permill,
			withdraw_fee: Permill,
		) -> DispatchResult {
			<T as Config>::AuthorityOrigin::ensure_origin(origin.clone())?;

			// Load state - return AssetNotFound if it does not exist
			let asset_state_a = OmnipoolPallet::<T>::load_asset_state(asset_a)?;
			let asset_state_b = OmnipoolPallet::<T>::load_asset_state(asset_b)?;

			// Create new subpool
			let pool_id = StableswapPallet::<T>::do_create_pool(
				share_asset.into(),
				&[asset_a.into(), asset_b.into()],
				amplification,
				trade_fee,
				withdraw_fee,
			)?;

			StableswapPallet::<T>::set_asset_tradability_state(
				pool_id,
				asset_a.into(),
				Self::to_stableswap_tradable(asset_state_a.tradable),
			);
			StableswapPallet::<T>::set_asset_tradability_state(
				pool_id,
				asset_b.into(),
				Self::to_stableswap_tradable(asset_state_b.tradable),
			);

			let omnipool_account = OmnipoolPallet::<T>::protocol_account();

			// Move liquidity from omnipool account to subpool
			StableswapPallet::<T>::move_liquidity_to_pool(
				&omnipool_account,
				pool_id,
				&[
					AssetLiquidity::<StableswapAssetIdOf<T>> {
						asset_id: asset_a.into(),
						amount: asset_state_a.reserve,
					},
					AssetLiquidity::<StableswapAssetIdOf<T>> {
						asset_id: asset_b.into(),
						amount: asset_state_b.reserve,
					},
				],
			)?;

			// Calculate stable asset states and migration details of each asset
			let subpool_state = hydra_dx_math::omnipool_subpools::create_new_subpool(
				&(&asset_state_a).into(),
				&(&asset_state_b).into(),
			)
			.ok_or(Error::<T>::Math)?;

			StableswapPallet::<T>::deposit_shares(&omnipool_account, pool_id, subpool_state.reserve)?;

			// Add Share token to omnipool as another asset - LRNA is Qi + Qj
			OmnipoolPallet::<T>::add_asset(
				pool_id.into(),
				(subpool_state, share_asset_weight_cap, Tradability::default()).into(),
			)?;

			let (asset_a_details, _) = hydra_dx_math::omnipool_subpools::calculate_asset_migration_details(
				&(asset_state_a).into(),
				None,
				Balance::zero(),
			)
			.ok_or(Error::<T>::Math)?;

			let asset_a_details: AssetDetail = asset_a_details.into();

			let (asset_b_details, _) = hydra_dx_math::omnipool_subpools::calculate_asset_migration_details(
				&(asset_state_b).into(),
				None,
				Balance::zero(),
			)
			.ok_or(Error::<T>::Math)?;

			let asset_b_details: AssetDetail = asset_b_details.into();

			// Remove assets from omnipool
			OmnipoolPallet::<T>::remove_asset(asset_a)?;
			OmnipoolPallet::<T>::remove_asset(asset_b)?;

			// Set states
			MigratedAssets::<T>::insert(asset_a, (pool_id, asset_a_details));
			MigratedAssets::<T>::insert(asset_b, (pool_id, asset_b_details));
			Subpools::<T>::insert(share_asset.into(), ());

			Self::deposit_event(Event::SubpoolCreated {
				id: pool_id,
				assets: (asset_a, asset_b),
			});

			Ok(())
		}

		/// Migrate omnipool asset to existing stableswap subpool.
		///
		/// Migration steps:
		/// - add asset to existing stableswap pool
		/// - set tradable state to preserve existing state
		/// - move liquidity from Omnipool account to subpool account
		/// - remove asset from Omnipool
		/// - store details to Subpool storage - MigratedAssets
		/// - update share aset state in Omnipool
		///
		/// Emits `AssetMigrated` event when successful
		#[pallet::call_index(1)]
		#[pallet::weight(<T as Config>::WeightInfo::migrate_asset_to_subpool())]
		pub fn migrate_asset_to_subpool(
			origin: OriginFor<T>,
			pool_id: StableswapAssetIdOf<T>,
			asset_id: AssetIdOf<T>,
		) -> DispatchResult {
			<T as Config>::AuthorityOrigin::ensure_origin(origin.clone())?;

			ensure!(Self::subpools(&pool_id).is_some(), Error::<T>::SubpoolNotFound);

			// Load asset state - returns AssetNotFound if it does not exist
			let asset_state = OmnipoolPallet::<T>::load_asset_state(asset_id)?;
			let subpool_state = OmnipoolPallet::<T>::load_asset_state(pool_id.into())?;
			let omnipool_account = OmnipoolPallet::<T>::protocol_account();

			StableswapPallet::<T>::add_asset_to_existing_pool(pool_id, asset_id.into())?;
			StableswapPallet::<T>::move_liquidity_to_pool(
				&omnipool_account,
				pool_id,
				&[AssetLiquidity::<StableswapAssetIdOf<T>> {
					asset_id: asset_id.into(),
					amount: asset_state.reserve,
				}],
			)?;
			StableswapPallet::<T>::set_asset_tradability_state(
				pool_id,
				asset_id.into(),
				Self::to_stableswap_tradable(asset_state.tradable),
			);
			OmnipoolPallet::<T>::remove_asset(asset_id)?;

			let share_issuance = CurrencyOf::<T>::total_issuance(pool_id.into());

			let (asset_details, share_state_change) =
				hydra_dx_math::omnipool_subpools::calculate_asset_migration_details(
					&(asset_state).into(),
					Some(&(subpool_state).into()),
					share_issuance,
				)
				.ok_or(Error::<T>::Math)?;

			let asset_details: AssetDetail = asset_details.into();
			let state_changes = share_state_change.ok_or(Error::<T>::Math)?;

			StableswapPallet::<T>::deposit_shares(&omnipool_account, pool_id, *state_changes.delta_reserve)?;

			//TODO: i wonder LRNA mint here ?

			OmnipoolPallet::<T>::update_asset_state(pool_id.into(), state_changes)?;

			MigratedAssets::<T>::insert(asset_id, (pool_id, asset_details));

			Self::deposit_event(Event::AssetMigrated { asset_id, pool_id });

			Ok(())
		}

		/// Add liquidity of asset with `asset_id` in quantity `amount` to Omnipool
		///
		/// `add_liquidity` adds specified asset amount to pool and in exchange gives the origin
		/// corresponding shares amount in form of NFT at current price.
		///
		/// Asset's tradable state is checked within corresponding pallet - Omnipool or Stableswap.
		///
		/// There can be 2 scenarios:
		/// 1. Adding omnipool asset
		/// 	- handled directly by omnipool pallet
		/// 2. Adding stable asset which has been migrated to subpool
		/// 	- liquidity is added to corresponding subpool
		/// 	- shares obtained are added as liquidity to Omnipool
		///
		/// Parameters:
		/// - `asset`: The identifier of the new asset added to the pool. Must be already in the pool
		/// - `amount`: Amount of asset added to omnipool
		///
		/// Events are emitted by Omnipool and Stableswap pallet.
		#[pallet::call_index(2)]
		#[pallet::weight(<T as Config>::WeightInfo::add_liquidity())]
		pub fn add_liquidity(origin: OriginFor<T>, asset_id: AssetIdOf<T>, amount: Balance) -> DispatchResult {
			let who = ensure_signed(origin.clone())?;

			if let Some((pool_id, _)) = MigratedAssets::<T>::get(&asset_id) {
				let shares = StableswapPallet::<T>::do_add_liquidity(
					&who,
					pool_id,
					&[AssetLiquidity {
						asset_id: asset_id.into(),
						amount,
					}],
				)?;
				OmnipoolPallet::<T>::add_liquidity(origin, pool_id.into(), shares)
			} else {
				OmnipoolPallet::<T>::add_liquidity(origin, asset_id, amount)
			}
		}

		/// Add liquidity of asset with `asset_id` in quantity `amount` to Omnipool
		///
		/// Asset must be migrated stable asset, otherwise error `NotStableAsset` is returned.
		///
		/// `add_liquidity_stable` adds liquidity of stable asset to subpool and liquidity provider
		/// can decide to keep the shares of stableswapool or add the shares to Omnipool in exchange of NFT.
		///
		/// Parameters:
		/// - `asset`: The identifier of the new asset added to the pool. Must be already in the pool
		/// - `amount`: Amount of asset added to omnipool
		/// - `mint_nft`: mint nft or keep the subpool shares
		///
		/// Events are emitted by Omnipool and Stableswap pallet.
		#[pallet::call_index(3)]
		#[pallet::weight(<T as Config>::WeightInfo::add_liquidity_stable())]
		pub fn add_liquidity_stable(
			origin: OriginFor<T>,
			asset_id: AssetIdOf<T>,
			amount: Balance,
			mint_nft: bool,
		) -> DispatchResult {
			let who = ensure_signed(origin.clone())?;

			if let Some((pool_id, _)) = MigratedAssets::<T>::get(&asset_id) {
				let shares = StableswapPallet::<T>::do_add_liquidity(
					&who,
					pool_id,
					&[AssetLiquidity {
						asset_id: asset_id.into(),
						amount,
					}],
				)?;

				if mint_nft {
					OmnipoolPallet::<T>::add_liquidity(origin, pool_id.into(), shares)
				} else {
					Ok(())
				}
			} else {
				Err(Error::<T>::NotStableAsset.into())
			}
		}

		/// Remove liquidity of asset with `asset_id` in quantity `amount` of shares from Omnipool or Subpool
		///
		/// `remove_liquidity` removes specified shares amount from given PositionId (NFT instance).
		///
		/// Asset's tradable state must contain REMOVE_LIQUIDITY flag, otherwise `NotAllowed` error is returned. Handled by Omnipool.
		///
		/// If withdrawing liquidity from subpool, it is required to specify which asset LP wants to withdraw.
		///
		/// In case position was created prior to asset migration, the position is converted into share asset position.
		///
		/// Parameters:
		/// - `position_id`: The identifier of position which liquidity is removed from.
		/// - `amount`: Amount of shares removed from omnipool
		/// - `asset`: Desired asset to withdraw from subpool
		///
		/// Events are emitted by Omnipool and Stableswap pallet.
		#[pallet::call_index(4)]
		#[pallet::weight(<T as Config>::WeightInfo::remove_liquidity())]
		pub fn remove_liquidity(
			origin: OriginFor<T>,
			position_id: T::PositionItemId,
			share_amount: Balance,
			asset: Option<AssetIdOf<T>>,
		) -> DispatchResult {
			let who = ensure_signed(origin.clone())?;

			let position = OmnipoolPallet::<T>::load_position(position_id, who.clone())?;

			let position = if let Some((pool_id, details)) = MigratedAssets::<T>::get(&position.asset_id) {
				let position = Self::convert_position(pool_id.into(), details, position)?;
				// Store the updated position
				OmnipoolPallet::<T>::set_position(position_id, &position)?;
				position
			} else {
				position
			};

			// Asset should be in isopool, call omnipool::remove_liquidity
			OmnipoolPallet::<T>::remove_liquidity(origin.clone(), position_id, share_amount)?;

			// TODO: should we allow just withdrawing subpool shares and keep them instead?

			match (Self::subpools(&position.asset_id.into()), asset) {
				(Some(_), Some(withdraw_asset)) => {
					let received = CurrencyOf::<T>::free_balance(position.asset_id, &who);
					StableswapPallet::<T>::remove_liquidity_one_asset(
						origin,
						position.asset_id.into(),
						withdraw_asset.into(),
						received,
					)
				}
				(Some(_), None) => Err(Error::<T>::WithdrawAssetNotSpecified.into()),
				_ => Ok(()),
			}
		}

		/// Execute a swap of `asset_in` for `asset_out`.
		///
		/// Asset's tradable states must contain SELL flag for asset_in and BUY flag for asset_out, otherwise `NotAllowed` error is returned.
		/// Handled by Omnipool and/or Stableswap pallets.
		///
		/// Different possible scenarios can occur:
		/// 1. Both asset_in and asset_out are in Omnipool
		/// 	- Omnipool's sell is invoked and trades is handled by the Omnipool pallet
		/// 2. Both asset_in and asset_out are in the Stableswap subpool
		/// 	- Stableswap's sell is invoked and trades is handled by the Stableswap pallet
		/// 3. asset_in and asset_out are in different subpools
		/// 	- Handled by swap implementation in subpool pallet
		/// 4. Asset_in is in Omnipool and asset_out is in Stableswap subpool
		/// 	- Handled by swap implementation in subpool pallet
		///
		/// Parameters:
		/// - `asset_in`: ID of asset sold to the pool
		/// - `asset_out`: ID of asset bought from the pool
		/// - `amount`: Amount of asset sold
		/// - `min_buy_amount`: Minimum amount required to receive
		///
		/// Emits `SellExecuted` event when successful if swap in handled by subpool pallet, otherwise events are
		/// emitted by Omnipool or Stableswap.
		#[pallet::call_index(5)]
		#[pallet::weight(<T as Config>::WeightInfo::sell())]
		pub fn sell(
			origin: OriginFor<T>,
			asset_in: AssetIdOf<T>,
			asset_out: AssetIdOf<T>,
			amount: Balance,
			min_buy_amount: Balance,
		) -> DispatchResult {
			let who = ensure_signed(origin.clone())?;

			match (MigratedAssets::<T>::get(asset_in), MigratedAssets::<T>::get(asset_out)) {
				(None, None) => {
					// both assets are omnipool assets
					OmnipoolPallet::<T>::sell(origin, asset_in, asset_out, amount, min_buy_amount)
				}
				(Some((pool_id_in, _)), Some((pool_id_out, _))) if pool_id_in == pool_id_out => {
					// both assets are migrated stable assets and in the same subpool
					StableswapPallet::<T>::sell(
						origin,
						pool_id_in,
						asset_in.into(),
						asset_out.into(),
						amount,
						min_buy_amount,
					)
				}
				(Some((pool_id_in, _)), Some((pool_id_out, _))) => {
					// both assets are migrated stable assets but in the different subpools
					Self::resolve_sell_between_subpools(
						&who,
						asset_in,
						asset_out,
						pool_id_in,
						pool_id_out,
						amount,
						min_buy_amount,
					)
				}
				(Some((pool_id_in, _)), None) => {
					// Selling stable asset and buy omnipool asset
					Self::resolve_mixed_trade_iso_out_given_stable_in(
						&who,
						asset_in,
						asset_out,
						pool_id_in,
						amount,
						min_buy_amount,
					)
				}
				(None, Some((pool_id_out, _))) => {
					// Sell omnipool asset and buy stable asset
					Self::resolve_mixed_trade_stable_out_given_asset_in(
						&who,
						asset_in,
						asset_out,
						pool_id_out,
						amount,
						min_buy_amount,
					)
				}
			}
		}

		/// Execute a swap of `asset_out` for `asset_in`.
		///
		/// Asset's tradable states must contain SELL flag for asset_in and BUY flag for asset_out, otherwise `NotAllowed` error is returned.
		/// Handled by Omnipool and/or Stableswap pallets.
		///
		/// Different possible scenarios can occur:
		/// 1. Both asset_in and asset_out are in Omnipool
		/// 	- Omnipool's sell is invoked and trades is handled by the Omnipool pallet
		/// 2. Both asset_in and asset_out are in the Stableswap subpool
		/// 	- Stableswap's sell is invoked and trades is handled by the Stableswap pallet
		/// 3. asset_in and asset_out are in different subpools
		/// 	- Handled by swap implementation in subpool pallet
		/// 4. Asset_in is in Omnipool and asset_out is in Stableswap subpool
		/// 	- Handled by swap implementation in subpool pallet
		///
		/// Parameters:
		/// - `asset_in`: ID of asset sold to the pool
		/// - `asset_out`: ID of asset bought from the pool
		/// - `amount`: Amount of asset sold
		/// - `min_buy_amount`: Minimum amount required to receive
		///
		/// Emits `SellExecuted` event when successful if swap in handled by subpool pallet, otherwise events are
		/// emitted by Omnipool or Stableswap.
		#[pallet::call_index(6)]
		#[pallet::weight(<T as Config>::WeightInfo::buy())]
		pub fn buy(
			origin: OriginFor<T>,
			asset_out: AssetIdOf<T>,
			asset_in: AssetIdOf<T>,
			amount: Balance,
			max_sell_amount: Balance,
		) -> DispatchResult {
			let who = ensure_signed(origin.clone())?;

			match (MigratedAssets::<T>::get(asset_in), MigratedAssets::<T>::get(asset_out)) {
				(None, None) => {
					// both assets are omnipool assets
					OmnipoolPallet::<T>::buy(origin, asset_out, asset_in, amount, max_sell_amount)
				}
				(Some((pool_id_in, _)), Some((pool_id_out, _))) if pool_id_in == pool_id_out => {
					// both assets are migrated stable assets and in the same subpool
					StableswapPallet::<T>::buy(
						origin,
						pool_id_in,
						asset_out.into(),
						asset_in.into(),
						amount,
						max_sell_amount,
					)
				}
				(Some((pool_id_in, _)), Some((pool_id_out, _))) => {
					// both assets are migrated stable assets but in the different subpools
					Self::resolve_buy_between_subpools(
						&who,
						asset_in,
						asset_out,
						pool_id_in,
						pool_id_out,
						amount,
						max_sell_amount,
					)
				}
				(Some((pool_id_in, _)), None) => {
					// Buy omnipool asset and sell stable asset
					Self::resolve_mixed_trade_stable_in_given_asset_out(
						&who,
						asset_in,
						asset_out,
						pool_id_in,
						amount,
						max_sell_amount,
					)
				}
				(None, Some((pool_id_out, _))) => {
					// Buy stablea _sset and sell omnipool asset
					Self::resolve_mixed_trade_iso_in_given_stable_out(
						&who,
						asset_in,
						asset_out,
						pool_id_out,
						amount,
						max_sell_amount,
					)
				}
			}
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {}
}

impl<T: Config> Pallet<T>
where
	<T as pallet_omnipool::Config>::AssetId:
		Into<<T as pallet_stableswap::Config>::AssetId> + From<<T as pallet_stableswap::Config>::AssetId>,
{
	/// Convert LP Omnipool position to Stableswap subpool position.
	///
	/// New position has asset_id to subpool id.
	fn convert_position(
		pool_id: <T as pallet_omnipool::Config>::AssetId,
		migration_details: AssetDetail,
		position: Position<Balance, <T as pallet_omnipool::Config>::AssetId>,
	) -> Result<Position<Balance, <T as pallet_omnipool::Config>::AssetId>, DispatchError> {
		let converted = hydra_dx_math::omnipool_subpools::convert_position(
			(&position).into(),
			MigrationDetails {
				price: migration_details.price,
				shares: migration_details.shares,
				hub_reserve: migration_details.hub_reserve,
				share_tokens: migration_details.share_tokens,
			},
		)
		.ok_or(Error::<T>::Math)?;

		Ok(Position {
			asset_id: pool_id,
			amount: converted.amount,
			shares: converted.shares,
			price: converted.price,
		})
	}

	/// Resolve buy trade between two different Stableswap subpools.
	#[require_transactional]
	fn resolve_buy_between_subpools(
		who: &T::AccountId,
		asset_in: AssetIdOf<T>,
		asset_out: AssetIdOf<T>,
		subpool_id_in: StableswapAssetIdOf<T>,
		subpool_id_out: StableswapAssetIdOf<T>,
		amount_out: Balance,
		max_limit: Balance,
	) -> DispatchResult {
		ensure!(
			StableswapPallet::<T>::is_asset_allowed(
				subpool_id_in,
				asset_in.into(),
				pallet_stableswap::types::Tradability::SELL
			) && StableswapPallet::<T>::is_asset_allowed(
				subpool_id_out,
				asset_out.into(),
				pallet_stableswap::types::Tradability::BUY
			),
			Error::<T>::NotAllowed
		);

		let subpool_in = StableswapPallet::<T>::get_pool(subpool_id_in)?;
		let subpool_out = StableswapPallet::<T>::get_pool(subpool_id_out)?;

		let idx_in = subpool_in
			.find_asset(asset_in.into())
			.ok_or(pallet_stableswap::Error::<T>::AssetNotInPool)?;
		let idx_out = subpool_out
			.find_asset(asset_out.into())
			.ok_or(pallet_stableswap::Error::<T>::AssetNotInPool)?;

		let share_asset_state_in = OmnipoolPallet::<T>::load_asset_state(subpool_id_in.into())?;
		let share_asset_state_out = OmnipoolPallet::<T>::load_asset_state(subpool_id_out.into())?;

		let share_issuance_in = CurrencyOf::<T>::total_issuance(subpool_id_in.into());
		let share_issuance_out = CurrencyOf::<T>::total_issuance(subpool_id_out.into());

		let asset_fee = <T as pallet_omnipool::Config>::AssetFee::get();
		let protocol_fee = <T as pallet_omnipool::Config>::ProtocolFee::get();
		let withdraw_fee = subpool_out.withdraw_fee;
		let current_imbalance = OmnipoolPallet::<T>::current_imbalance();

		// Calculate how much shares to remove if amount out is remove from subpool
		let delta_u = calculate_shares_removed::<MAX_D_ITERATIONS>(
			&subpool_out.balances::<T>(),
			idx_out,
			amount_out,
			subpool_out.amplification as u128,
			share_issuance_out,
			withdraw_fee,
		)
		.ok_or(Error::<T>::Math)?;

		// Buy delta_u ( share amount) from omnipool
		let buy_changes = calculate_buy_state_changes(
			&(&share_asset_state_in).into(),
			&(&share_asset_state_out).into(),
			delta_u,
			asset_fee,
			protocol_fee,
			current_imbalance.value,
		)
		.ok_or(Error::<T>::Math)?;

		// calculate how much to add if we add given amount of shares
		let delta_t_j = calculate_amount_to_add_for_shares::<MAX_D_ITERATIONS>(
			&subpool_in.balances::<T>(),
			idx_in,
			*buy_changes.asset_in.delta_reserve,
			subpool_in.amplification as u128,
			share_issuance_in,
		)
		.ok_or(Error::<T>::Math)?;

		ensure!(delta_t_j <= max_limit, Error::<T>::LimitExceeded);

		// Update subpools - transfer between subpool and who
		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_in.into(),
			who,
			&subpool_in.pool_account::<T>(),
			delta_t_j,
		)?;
		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_out.into(),
			&subpool_out.pool_account::<T>(),
			who,
			amount_out,
		)?;

		// Update share asset state in omnipool- mint/burn share asset
		<T as pallet_omnipool::Config>::Currency::withdraw(
			subpool_id_out.into(),
			&OmnipoolPallet::<T>::protocol_account(),
			*buy_changes.asset_out.delta_reserve,
		)?;

		<T as pallet_omnipool::Config>::Currency::deposit(
			subpool_id_in.into(),
			&OmnipoolPallet::<T>::protocol_account(),
			*buy_changes.asset_in.delta_reserve,
		)?;

		OmnipoolPallet::<T>::update_omnipool_state_given_trade_result(
			subpool_id_in.into(),
			subpool_id_out.into(),
			buy_changes,
		)?;

		Ok(())
	}

	/// Resolve sell trade between two different Stableswap subpools.
	#[require_transactional]
	fn resolve_sell_between_subpools(
		who: &T::AccountId,
		asset_in: AssetIdOf<T>,
		asset_out: AssetIdOf<T>,
		subpool_id_in: StableswapAssetIdOf<T>,
		subpool_id_out: StableswapAssetIdOf<T>,
		amount_in: Balance,
		min_limit: Balance,
	) -> DispatchResult {
		ensure!(
			StableswapPallet::<T>::is_asset_allowed(
				subpool_id_in,
				asset_in.into(),
				pallet_stableswap::types::Tradability::SELL
			) && StableswapPallet::<T>::is_asset_allowed(
				subpool_id_out,
				asset_out.into(),
				pallet_stableswap::types::Tradability::BUY
			),
			Error::<T>::NotAllowed
		);

		let subpool_in = StableswapPallet::<T>::get_pool(subpool_id_in)?;
		let subpool_out = StableswapPallet::<T>::get_pool(subpool_id_out)?;

		let idx_in = subpool_in
			.find_asset(asset_in.into())
			.ok_or(pallet_stableswap::Error::<T>::AssetNotInPool)?;
		let idx_out = subpool_out
			.find_asset(asset_out.into())
			.ok_or(pallet_stableswap::Error::<T>::AssetNotInPool)?;

		let share_asset_state_in = OmnipoolPallet::<T>::load_asset_state(subpool_id_in.into())?;
		let share_asset_state_out = OmnipoolPallet::<T>::load_asset_state(subpool_id_out.into())?;

		let share_issuance_in = CurrencyOf::<T>::total_issuance(subpool_id_in.into());
		let share_issuance_out = CurrencyOf::<T>::total_issuance(subpool_id_out.into());

		let asset_fee = <T as pallet_omnipool::Config>::AssetFee::get();
		let protocol_fee = <T as pallet_omnipool::Config>::ProtocolFee::get();
		let withdraw_fee = subpool_out.withdraw_fee;
		let current_imbalance = OmnipoolPallet::<T>::current_imbalance();

		// Calculate how much shares to add if we add given amount of asset
		let delta_u = calculate_shares_for_amount::<MAX_D_ITERATIONS>(
			&subpool_in.balances::<T>(),
			idx_in,
			amount_in,
			subpool_in.amplification as u128,
			share_issuance_in,
		)
		.ok_or(Error::<T>::Math)?;

		// Sell the share amount to omnipool, receive shares of the second subpool
		let sell_changes = calculate_sell_state_changes(
			&(&share_asset_state_in).into(),
			&(&share_asset_state_out).into(),
			delta_u,
			asset_fee,
			protocol_fee,
			current_imbalance.value,
		)
		.ok_or(Error::<T>::Math)?;

		// Calculate amount of asset to remove if we remove given amount of shares
		let (delta_t_j, f) = calculate_withdraw_one_asset::<MAX_D_ITERATIONS, MAX_Y_ITERATIONS>(
			&subpool_out.balances::<T>(),
			*sell_changes.asset_out.delta_reserve,
			idx_out,
			share_issuance_out,
			subpool_out.amplification as u128,
			withdraw_fee,
		)
		.ok_or(Error::<T>::Math)?;

		let delta_t_j = delta_t_j.checked_sub(f).ok_or(Error::<T>::Math)?;

		ensure!(delta_t_j >= min_limit, Error::<T>::LimitNotReached);

		// Update subpools - transfer between subpool and who
		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_in.into(),
			who,
			&subpool_in.pool_account::<T>(),
			amount_in,
		)?;
		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_out.into(),
			&subpool_out.pool_account::<T>(),
			who,
			delta_t_j,
		)?;

		<T as pallet_omnipool::Config>::Currency::withdraw(
			subpool_id_out.into(),
			&OmnipoolPallet::<T>::protocol_account(),
			*sell_changes.asset_out.delta_reserve,
		)?;
		<T as pallet_omnipool::Config>::Currency::deposit(
			subpool_id_in.into(),
			&OmnipoolPallet::<T>::protocol_account(),
			*sell_changes.asset_in.delta_reserve,
		)?;

		OmnipoolPallet::<T>::update_omnipool_state_given_trade_result(
			subpool_id_in.into(),
			subpool_id_out.into(),
			sell_changes,
		)?;

		Ok(())
	}

	/// Resolve sell trade between subpool and Omnipool where asset in is stable asset and asset out is omnipool asset.
	#[require_transactional]
	fn resolve_mixed_trade_iso_out_given_stable_in(
		who: &T::AccountId,
		asset_in: AssetIdOf<T>,                // stable asset
		asset_out: AssetIdOf<T>,               // omnipool asset
		subpool_id_in: StableswapAssetIdOf<T>, // pool id in which the stable asset is
		amount_in: Balance,
		min_limit: Balance,
	) -> DispatchResult {
		if asset_out == <T as pallet_omnipool::Config>::HubAssetId::get() {
			// LRNA is not allowed to be bought
			return Err(pallet_omnipool::Error::<T>::NotAllowed.into());
		}

		let asset_state_out = OmnipoolPallet::<T>::load_asset_state(asset_out)?;
		let share_state_in = OmnipoolPallet::<T>::load_asset_state(subpool_id_in.into())?;

		ensure!(
			StableswapPallet::<T>::is_asset_allowed(
				subpool_id_in,
				asset_in.into(),
				pallet_stableswap::types::Tradability::SELL
			) && asset_state_out.tradable.contains(Tradability::BUY),
			Error::<T>::NotAllowed
		);

		let subpool_in = StableswapPallet::<T>::get_pool(subpool_id_in)?;

		let share_issuance_in = CurrencyOf::<T>::total_issuance(subpool_id_in.into());

		let asset_fee = <T as pallet_omnipool::Config>::AssetFee::get();
		let protocol_fee = <T as pallet_omnipool::Config>::ProtocolFee::get();
		let withdraw_fee = subpool_in.withdraw_fee;
		let current_imbalance = OmnipoolPallet::<T>::current_imbalance();

		let idx_in = subpool_in
			.find_asset(asset_in.into())
			.ok_or(pallet_stableswap::Error::<T>::AssetNotInPool)?;

		let delta_u = calculate_shares_for_amount::<MAX_D_ITERATIONS>(
			&subpool_in.balances::<T>(),
			idx_in,
			amount_in,
			subpool_in.amplification as u128,
			share_issuance_in,
		)
		.ok_or(Error::<T>::Math)?;

		let sell_changes = calculate_sell_state_changes(
			&(&share_state_in).into(),
			&(&asset_state_out).into(),
			delta_u,
			asset_fee,
			protocol_fee,
			current_imbalance.value,
		)
		.ok_or(Error::<T>::Math)?;

		ensure!(
			*sell_changes.asset_out.delta_reserve >= min_limit,
			Error::<T>::LimitNotReached
		);

		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_in.into(),
			who,
			&subpool_in.pool_account::<T>(),
			amount_in,
		)?;
		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_out.into(),
			&OmnipoolPallet::<T>::protocol_account(),
			who,
			*sell_changes.asset_out.delta_reserve,
		)?;

		<T as pallet_omnipool::Config>::Currency::deposit(
			subpool_id_in.into(),
			&OmnipoolPallet::<T>::protocol_account(),
			*sell_changes.asset_in.delta_reserve,
		)?;

		OmnipoolPallet::<T>::update_omnipool_state_given_trade_result(subpool_id_in.into(), asset_out, sell_changes)?;

		Ok(())
	}

	/// Handle sell trade between subpool and omnipool where asset in is omnipool asset and asset out is stable asset.
	#[require_transactional]
	fn resolve_mixed_trade_stable_out_given_asset_in(
		who: &T::AccountId,
		asset_in: AssetIdOf<T>,                 // omnipool asset
		asset_out: AssetIdOf<T>,                // stable asset
		subpool_id_out: StableswapAssetIdOf<T>, // pool id in which the stable asset is
		amount_in: Balance,
		min_limit: Balance,
	) -> DispatchResult {
		if asset_in == <T as pallet_omnipool::Config>::HubAssetId::get() {
			return Self::resolve_mixed_trade_stable_out_given_hub_asset_in(
				who,
				asset_in,
				asset_out,
				subpool_id_out,
				amount_in,
				min_limit,
			);
		}

		let asset_state_in = OmnipoolPallet::<T>::load_asset_state(asset_in)?;
		let share_state_out = OmnipoolPallet::<T>::load_asset_state(subpool_id_out.into())?;

		ensure!(
			StableswapPallet::<T>::is_asset_allowed(
				subpool_id_out,
				asset_out.into(),
				pallet_stableswap::types::Tradability::BUY
			) && asset_state_in.tradable.contains(Tradability::SELL),
			Error::<T>::NotAllowed
		);

		let subpool_out = StableswapPallet::<T>::get_pool(subpool_id_out)?;

		let share_issuance_out = CurrencyOf::<T>::total_issuance(subpool_id_out.into());

		let asset_fee = <T as pallet_omnipool::Config>::AssetFee::get();
		let protocol_fee = <T as pallet_omnipool::Config>::ProtocolFee::get();
		let withdraw_fee = subpool_out.withdraw_fee;
		let current_imbalance = OmnipoolPallet::<T>::current_imbalance();

		let idx_out = subpool_out
			.find_asset(asset_out.into())
			.ok_or(pallet_stableswap::Error::<T>::AssetNotInPool)?;

		let sell_changes = calculate_sell_state_changes(
			&(&asset_state_in).into(),
			&(&share_state_out).into(),
			amount_in,
			asset_fee,
			protocol_fee,
			current_imbalance.value,
		)
		.ok_or(Error::<T>::Math)?;

		let (delta_t_j, f) = calculate_withdraw_one_asset::<MAX_D_ITERATIONS, MAX_Y_ITERATIONS>(
			&subpool_out.balances::<T>(),
			*sell_changes.asset_out.delta_reserve,
			idx_out,
			share_issuance_out,
			subpool_out.amplification as u128,
			withdraw_fee,
		)
		.ok_or(Error::<T>::Math)?;

		let delta_t_j = delta_t_j.checked_sub(f).ok_or(Error::<T>::Math)?;

		ensure!(delta_t_j >= min_limit, Error::<T>::LimitNotReached);

		debug_assert_eq!(
			*sell_changes.asset_in.delta_reserve, amount_in,
			"Returned amount is not equal to amount_in"
		);

		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_in.into(),
			who,
			&OmnipoolPallet::<T>::protocol_account(),
			*sell_changes.asset_in.delta_reserve,
		)?;

		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_out.into(),
			&subpool_out.pool_account::<T>(),
			who,
			delta_t_j,
		)?;

		<T as pallet_omnipool::Config>::Currency::withdraw(
			subpool_id_out.into(),
			&OmnipoolPallet::<T>::protocol_account(),
			*sell_changes.asset_out.delta_reserve,
		)?;

		OmnipoolPallet::<T>::update_omnipool_state_given_trade_result(asset_in, subpool_id_out.into(), sell_changes)?;

		Ok(())
	}

	/// Handle sell trade between subpool and omnipool where asset in is hub asset and asset out is stable asset.
	#[require_transactional]
	fn resolve_mixed_trade_stable_out_given_hub_asset_in(
		who: &T::AccountId,
		asset_in: AssetIdOf<T>,                 // omnipool asset
		asset_out: AssetIdOf<T>,                // stable asset
		subpool_id_out: StableswapAssetIdOf<T>, // pool id in which the stable asset is
		amount_in: Balance,
		min_limit: Balance,
	) -> DispatchResult {
		ensure!(
			asset_in == <T as pallet_omnipool::Config>::HubAssetId::get(),
			pallet_omnipool::Error::<T>::NotAllowed
		);

		let share_state_out = OmnipoolPallet::<T>::load_asset_state(subpool_id_out.into())?;
		let subpool_out = StableswapPallet::<T>::get_pool(subpool_id_out)?;
		ensure!(
			StableswapPallet::<T>::is_asset_allowed(
				subpool_id_out,
				asset_out.into(),
				pallet_stableswap::types::Tradability::BUY
			) && OmnipoolPallet::<T>::is_hub_asset_allowed(Tradability::SELL),
			Error::<T>::NotAllowed
		);

		let share_issuance_out = CurrencyOf::<T>::total_issuance(subpool_id_out.into());

		let asset_fee = <T as pallet_omnipool::Config>::AssetFee::get();
		let withdraw_fee = subpool_out.withdraw_fee;
		let current_imbalance = OmnipoolPallet::<T>::current_imbalance();
		let current_hub_asset_liquidity = CurrencyOf::<T>::free_balance(
			<T as pallet_omnipool::Config>::HubAssetId::get(),
			&OmnipoolPallet::<T>::protocol_account(),
		);

		let idx_out = subpool_out
			.find_asset(asset_out.into())
			.ok_or(pallet_stableswap::Error::<T>::AssetNotInPool)?;

		let sell_changes = calculate_sell_hub_state_changes(
			&(&share_state_out).into(),
			amount_in,
			asset_fee,
			I129 {
				value: current_imbalance.value,
				negative: true,
			},
			current_hub_asset_liquidity,
		)
		.ok_or(Error::<T>::Math)?;

		let (delta_t_j, f) = calculate_withdraw_one_asset::<MAX_D_ITERATIONS, MAX_Y_ITERATIONS>(
			&subpool_out.balances::<T>(),
			*sell_changes.asset.delta_reserve,
			idx_out,
			share_issuance_out,
			subpool_out.amplification as u128,
			withdraw_fee,
		)
		.ok_or(Error::<T>::Math)?;

		let delta_t_j = delta_t_j.checked_sub(f).ok_or(Error::<T>::Math)?;

		ensure!(delta_t_j >= min_limit, Error::<T>::LimitNotReached);

		debug_assert_eq!(
			*sell_changes.asset.delta_hub_reserve, amount_in,
			"Returned amount is not equal to amount_in"
		);

		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_in.into(),
			who,
			&OmnipoolPallet::<T>::protocol_account(),
			*sell_changes.asset.delta_hub_reserve,
		)?;
		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_out.into(),
			&subpool_out.pool_account::<T>(),
			who,
			delta_t_j,
		)?;

		<T as pallet_omnipool::Config>::Currency::withdraw(
			subpool_id_out.into(),
			&OmnipoolPallet::<T>::protocol_account(),
			*sell_changes.asset.delta_reserve,
		)?;

		OmnipoolPallet::<T>::update_omnipool_state_given_hub_asset_trade(subpool_id_out.into(), sell_changes)?;

		Ok(())
	}

	/// Handle buy itrade between subpool and omnipool where asset in is stable asset and asset out is omnipool asset.
	#[require_transactional]
	fn resolve_mixed_trade_stable_in_given_asset_out(
		who: &T::AccountId,
		asset_in: AssetIdOf<T>,                // stable asset
		asset_out: AssetIdOf<T>,               // omnipool asset
		subpool_id_in: StableswapAssetIdOf<T>, // pool id in which the stable asset is
		amount_out: Balance,
		max_limit: Balance,
	) -> DispatchResult {
		if asset_out == <T as pallet_omnipool::Config>::HubAssetId::get() {
			// LRNA is not allowed to be bought
			return Err(pallet_omnipool::Error::<T>::NotAllowed.into());
		}

		let asset_state = OmnipoolPallet::<T>::load_asset_state(asset_out)?;
		let share_state = OmnipoolPallet::<T>::load_asset_state(subpool_id_in.into())?;

		ensure!(
			StableswapPallet::<T>::is_asset_allowed(
				subpool_id_in,
				asset_in.into(),
				pallet_stableswap::types::Tradability::SELL
			) && asset_state.tradable.contains(Tradability::BUY),
			Error::<T>::NotAllowed
		);

		let subpool_in = StableswapPallet::<T>::get_pool(subpool_id_in)?;

		let share_issuance_in = CurrencyOf::<T>::total_issuance(subpool_id_in.into());

		let asset_fee = <T as pallet_omnipool::Config>::AssetFee::get();
		let protocol_fee = <T as pallet_omnipool::Config>::ProtocolFee::get();
		let withdraw_fee = subpool_in.withdraw_fee;
		let current_imbalance = OmnipoolPallet::<T>::current_imbalance();

		let idx_in = subpool_in
			.find_asset(asset_in.into())
			.ok_or(pallet_stableswap::Error::<T>::AssetNotInPool)?;

		let buy_changes = calculate_buy_state_changes(
			&(&share_state).into(),
			&(&asset_state).into(),
			amount_out,
			asset_fee,
			protocol_fee,
			current_imbalance.value,
		)
		.ok_or(Error::<T>::Math)?;

		let delta_t_j = calculate_amount_to_add_for_shares::<MAX_D_ITERATIONS>(
			&subpool_in.balances::<T>(),
			idx_in,
			*buy_changes.asset_in.delta_reserve,
			subpool_in.amplification as u128,
			share_issuance_in,
		)
		.ok_or(Error::<T>::Math)?;

		ensure!(delta_t_j <= max_limit, Error::<T>::LimitExceeded);

		debug_assert_eq!(
			*buy_changes.asset_out.delta_reserve, amount_out,
			"Returned amount is not equal to amount_out"
		);

		// Update subpools - transfer between subpool and who
		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_in.into(),
			who,
			&subpool_in.pool_account::<T>(),
			delta_t_j,
		)?;
		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_out.into(),
			&OmnipoolPallet::<T>::protocol_account(),
			who,
			*buy_changes.asset_out.delta_reserve,
		)?;

		<T as pallet_omnipool::Config>::Currency::deposit(
			subpool_id_in.into(),
			&OmnipoolPallet::<T>::protocol_account(),
			*buy_changes.asset_in.delta_reserve,
		)?;

		OmnipoolPallet::<T>::update_omnipool_state_given_trade_result(subpool_id_in.into(), asset_out, buy_changes)?;

		Ok(())
	}

	/// Resolve buy trade between subpool and omnipool where asset in is omnipool asset and asset out is stable asset.
	#[require_transactional]
	fn resolve_mixed_trade_iso_in_given_stable_out(
		who: &T::AccountId,
		asset_in: AssetIdOf<T>,                 // omnipool asset
		asset_out: AssetIdOf<T>,                // stable asset
		subpool_id_out: StableswapAssetIdOf<T>, // pool id in which the stable asset is
		amount_out: Balance,
		max_limit: Balance,
	) -> DispatchResult {
		if asset_in == <T as pallet_omnipool::Config>::HubAssetId::get() {
			return Self::resolve_mixed_trade_hub_asset_in_given_stable_out(
				who,
				asset_in,
				asset_out,
				subpool_id_out,
				amount_out,
				max_limit,
			);
		}

		let asset_state_in = OmnipoolPallet::<T>::load_asset_state(asset_in)?;
		let share_state_out = OmnipoolPallet::<T>::load_asset_state(subpool_id_out.into())?;

		ensure!(
			StableswapPallet::<T>::is_asset_allowed(
				subpool_id_out,
				asset_out.into(),
				pallet_stableswap::types::Tradability::BUY
			) && asset_state_in.tradable.contains(Tradability::SELL),
			Error::<T>::NotAllowed
		);

		let subpool_out = StableswapPallet::<T>::get_pool(subpool_id_out)?;

		let share_issuance_out = CurrencyOf::<T>::total_issuance(subpool_id_out.into());

		let asset_fee = <T as pallet_omnipool::Config>::AssetFee::get();
		let protocol_fee = <T as pallet_omnipool::Config>::ProtocolFee::get();
		let withdraw_fee = subpool_out.withdraw_fee;
		let current_imbalance = OmnipoolPallet::<T>::current_imbalance();

		let idx_out = subpool_out
			.find_asset(asset_out.into())
			.ok_or(pallet_stableswap::Error::<T>::AssetNotInPool)?;

		let delta_u = calculate_shares_removed::<MAX_D_ITERATIONS>(
			&subpool_out.balances::<T>(),
			idx_out,
			amount_out,
			subpool_out.amplification as u128,
			share_issuance_out,
			withdraw_fee,
		)
		.ok_or(Error::<T>::Math)?;

		let buy_changes = calculate_buy_state_changes(
			&(&asset_state_in).into(),
			&(&share_state_out).into(),
			delta_u,
			asset_fee,
			protocol_fee,
			current_imbalance.value,
		)
		.ok_or(Error::<T>::Math)?;

		ensure!(
			*buy_changes.asset_in.delta_reserve <= max_limit,
			Error::<T>::LimitExceeded
		);

		// Update subpools - transfer between subpool and who
		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_in.into(),
			who,
			&OmnipoolPallet::<T>::protocol_account(),
			*buy_changes.asset_in.delta_reserve,
		)?;
		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_out.into(),
			&subpool_out.pool_account::<T>(),
			who,
			amount_out,
		)?;

		<T as pallet_omnipool::Config>::Currency::withdraw(
			subpool_id_out.into(),
			&OmnipoolPallet::<T>::protocol_account(),
			*buy_changes.asset_out.delta_reserve,
		)?;

		OmnipoolPallet::<T>::update_omnipool_state_given_trade_result(asset_in, subpool_id_out.into(), buy_changes)?;

		Ok(())
	}

	/// Resolve buy trade between subpool and omnipool where asset in is hub asset and asset out is stable asset.
	#[require_transactional]
	fn resolve_mixed_trade_hub_asset_in_given_stable_out(
		who: &T::AccountId,
		asset_in: AssetIdOf<T>,                 // omnipool asset
		asset_out: AssetIdOf<T>,                // stable asset
		subpool_id_out: StableswapAssetIdOf<T>, // pool id in which the stable asset is
		amount_out: Balance,
		max_limit: Balance,
	) -> DispatchResult {
		ensure!(
			asset_in == <T as pallet_omnipool::Config>::HubAssetId::get(),
			pallet_omnipool::Error::<T>::NotAllowed
		);

		let share_state_out = OmnipoolPallet::<T>::load_asset_state(subpool_id_out.into())?;
		let subpool_out = StableswapPallet::<T>::get_pool(subpool_id_out)?;

		ensure!(
			StableswapPallet::<T>::is_asset_allowed(
				subpool_id_out,
				asset_out.into(),
				pallet_stableswap::types::Tradability::BUY
			) && OmnipoolPallet::<T>::is_hub_asset_allowed(Tradability::SELL),
			Error::<T>::NotAllowed
		);

		let share_issuance_out = CurrencyOf::<T>::total_issuance(subpool_id_out.into());

		let asset_fee = <T as pallet_omnipool::Config>::AssetFee::get();
		let withdraw_fee = subpool_out.withdraw_fee;
		let current_imbalance = OmnipoolPallet::<T>::current_imbalance();
		let current_hub_asset_liquidity = CurrencyOf::<T>::free_balance(
			<T as pallet_omnipool::Config>::HubAssetId::get(),
			&OmnipoolPallet::<T>::protocol_account(),
		);

		let idx_out = subpool_out
			.find_asset(asset_out.into())
			.ok_or(pallet_stableswap::Error::<T>::AssetNotInPool)?;

		let delta_u = calculate_shares_removed::<MAX_D_ITERATIONS>(
			&subpool_out.balances::<T>(),
			idx_out,
			amount_out,
			subpool_out.amplification as u128,
			share_issuance_out,
			withdraw_fee,
		)
		.ok_or(Error::<T>::Math)?;

		let buy_changes = calculate_buy_for_hub_asset_state_changes(
			&(&share_state_out).into(),
			delta_u,
			asset_fee,
			I129 {
				value: current_imbalance.value,
				negative: true,
			},
			current_hub_asset_liquidity,
		)
		.ok_or(Error::<T>::Math)?;

		ensure!(*buy_changes.asset.delta_reserve <= max_limit, Error::<T>::LimitExceeded);

		// Update subpools - transfer between subpool and who
		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_in.into(),
			who,
			&OmnipoolPallet::<T>::protocol_account(),
			*buy_changes.asset.delta_hub_reserve,
		)?;
		<T as pallet_stableswap::Config>::Currency::transfer(
			asset_out.into(),
			&subpool_out.pool_account::<T>(),
			who,
			amount_out,
		)?;

		<T as pallet_omnipool::Config>::Currency::withdraw(
			subpool_id_out.into(),
			&OmnipoolPallet::<T>::protocol_account(),
			*buy_changes.asset.delta_reserve,
		)?;

		OmnipoolPallet::<T>::update_omnipool_state_given_hub_asset_trade(subpool_id_out.into(), buy_changes)?;

		Ok(())
	}

	fn to_stableswap_tradable(omnipool_state: Tradability) -> pallet_stableswap::types::Tradability {
		pallet_stableswap::types::Tradability::from_bits_truncate(omnipool_state.bits())
	}
}
