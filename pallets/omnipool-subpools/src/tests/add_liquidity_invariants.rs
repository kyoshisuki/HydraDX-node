use super::*;
use crate::*;
use proptest::prelude::*;

proptest! {
	//Spec: https://www.notion.so/Add-Liquidity-to-stableswap-subpool-d3983e19dd7c4de9b284c74c317be02c#da9e063badf5428bbce53a798df14e48
	#![proptest_config(ProptestConfig::with_cases(1))]
	#[test]
	fn add_liquidity_invariants(
		new_liquidity_amount in asset_reserve(),
		asset_3 in pool_token(ASSET_3),
		asset_4 in pool_token(ASSET_4),
	) {
			ExtBuilder::default()
				.with_registered_asset(ASSET_3)
				.with_registered_asset(ASSET_4)
				.with_registered_asset(SHARE_ASSET_AS_POOL_ID)
				.add_endowed_accounts((LP1, 1_000, 5000 * ONE))
				.add_endowed_accounts((Omnipool::protocol_account(), ASSET_3, 3000 * ONE))
				.add_endowed_accounts((Omnipool::protocol_account(), ASSET_4, 4000 * ONE))
				.add_endowed_accounts((ALICE, ASSET_3, new_liquidity_amount))
				.with_initial_pool(FixedU128::from_float(0.5), FixedU128::from(1))
				.build()
				.execute_with(|| {
					add_omnipool_token!(ASSET_3);
					add_omnipool_token!(ASSET_4);

					create_subpool!(SHARE_ASSET_AS_POOL_ID, ASSET_3, ASSET_4);

					let pool_account = AccountIdConstructor::from_assets(&vec![ASSET_3, ASSET_4], None);
					let omnipool_account = Omnipool::protocol_account();

					let stableswap_pool_share_asset_before = Omnipool::load_asset_state(SHARE_ASSET_AS_POOL_ID).unwrap();


					//Act
					let position_id: u32 = Omnipool::next_position_id();
					let new_liquidity = 100 * ONE;
					assert_ok!(OmnipoolSubpools::add_liquidity(
						Origin::signed(ALICE),
						ASSET_3,
						new_liquidity
					));


					//Assert
					let stableswap_pool_share_asset_after = Omnipool::load_asset_state(SHARE_ASSET_AS_POOL_ID).unwrap();
					assert_eq!(stableswap_pool_share_asset_after.reserve - stableswap_pool_share_asset_after.shares,stableswap_pool_share_asset_before.reserve - stableswap_pool_share_asset_before.shares);
			});
	}
}
