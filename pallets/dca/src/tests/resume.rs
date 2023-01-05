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

use crate::tests::mock::*;
use crate::tests::*;
use crate::{AssetId, Bond};
use crate::{Error, Event, Order, PoolType, Recurrence, Schedule, ScheduleId, Trade};
use frame_support::traits::OnInitialize;
use frame_support::{assert_noop, assert_ok};
use frame_system::pallet_prelude::BlockNumberFor;
use orml_traits::MultiReservableCurrency;
use pretty_assertions::assert_eq;
use sp_runtime::traits::ConstU32;
use sp_runtime::BoundedVec;
use sp_runtime::DispatchError;
use sp_runtime::DispatchError::BadOrigin;
use test_case::test_case;

#[test]
fn resume_should_fail_when_called_by_non_owner() {
	ExtBuilder::default()
		.with_endowed_accounts(vec![(ALICE, HDX, 10000 * ONE)])
		.build()
		.execute_with(|| {
			//Arrange
			let schedule = ScheduleBuilder::new().with_recurrence(Recurrence::Fixed(5)).build();
			set_block_number(500);
			assert_ok!(DCA::schedule(Origin::signed(ALICE), schedule, Option::None));

			//Act and assert
			let schedule_id = 1;
			assert_noop!(
				DCA::resume(Origin::signed(BOB), schedule_id, Option::None),
				Error::<Test>::NotScheduleOwner
			);
		});
}

#[test]
fn resume_should_schedule_to_next_block_when_next_execution_block_is_not_defined() {
	ExtBuilder::default()
		.with_endowed_accounts(vec![(ALICE, HDX, 10000 * ONE)])
		.build()
		.execute_with(|| {
			//Arrange
			let schedule = ScheduleBuilder::new().with_recurrence(Recurrence::Fixed(5)).build();
			set_block_number(500);
			assert_ok!(DCA::schedule(Origin::signed(ALICE), schedule, Option::None));

			let schedule_id = 1;
			assert_ok!(DCA::pause(Origin::signed(ALICE), schedule_id, 501));

			//Act
			let schedule_id = 1;
			assert_ok!(DCA::resume(Origin::signed(ALICE), schedule_id, Option::None));

			//Assert
			let schedule_ids = DCA::schedule_ids_per_block(501);
			assert!(DCA::schedule_ids_per_block(501).is_some());
			let expected_scheduled_ids_for_next_block = create_bounded_vec_with_schedule_ids(vec![1]);
			assert_eq!(schedule_ids.unwrap(), expected_scheduled_ids_for_next_block);
		});
}

#[test]
fn resume_should_schedule_to_next_block_when_there_is_already_existing_schedule_in_next_block() {
	ExtBuilder::default()
		.with_endowed_accounts(vec![(ALICE, HDX, 10000 * ONE), (BOB, HDX, 10000 * ONE)])
		.build()
		.execute_with(|| {
			//Arrange
			let schedule = ScheduleBuilder::new().with_recurrence(Recurrence::Fixed(5)).build();
			let schedule2 = ScheduleBuilder::new().with_recurrence(Recurrence::Fixed(5)).build();
			set_block_number(500);
			assert_ok!(DCA::schedule(Origin::signed(ALICE), schedule, Option::None));
			assert_ok!(DCA::schedule(Origin::signed(BOB), schedule2, Option::None));
			assert_scheduled_ids(501, vec![1, 2]);

			let schedule_id = 1;
			assert_ok!(DCA::pause(Origin::signed(ALICE), schedule_id, 501));
			assert_scheduled_ids(501, vec![2]);

			//Act
			let schedule_id = 1;
			assert_ok!(DCA::resume(Origin::signed(ALICE), schedule_id, Option::None));

			//Assert
			assert_scheduled_ids(501, vec![2, 1]);
		});
}

#[test_case(1)]
#[test_case(499)]
#[test_case(500)]
fn resume_should_fail_when_specified_next_block_is_equal_to_current_block(block: BlockNumberFor<Test>) {
	ExtBuilder::default()
		.with_endowed_accounts(vec![(ALICE, HDX, 10000 * ONE)])
		.build()
		.execute_with(|| {
			//Arrange
			let schedule = ScheduleBuilder::new().with_recurrence(Recurrence::Fixed(5)).build();
			set_block_number(500);
			assert_ok!(DCA::schedule(Origin::signed(ALICE), schedule, Option::None));

			let schedule_id = 1;
			assert_ok!(DCA::pause(Origin::signed(ALICE), schedule_id, 501));

			//Act
			set_block_number(501);
			let schedule_id = 1;
			assert_noop!(
				DCA::resume(Origin::signed(ALICE), schedule_id, Option::Some(block)),
				Error::<Test>::BlockNumberIsNotInFuture
			);
		});
}

fn assert_scheduled_ids(block: BlockNumberFor<Test>, expected_schedule_ids: Vec<ScheduleId>) {
	let actual_schedule_ids = DCA::schedule_ids_per_block(block);
	assert!(DCA::schedule_ids_per_block(block).is_some());
	let expected_scheduled_ids_for_next_block = create_bounded_vec_with_schedule_ids(expected_schedule_ids);
	assert_eq!(actual_schedule_ids.unwrap(), expected_scheduled_ids_for_next_block);
}

//TODO: add test to check if the schedule is indeed suspendeed. if not then error

fn create_bounded_vec_with_schedule_ids(schedule_ids: Vec<ScheduleId>) -> BoundedVec<ScheduleId, ConstU32<5>> {
	let bounded_vec: BoundedVec<ScheduleId, sp_runtime::traits::ConstU32<5>> = schedule_ids.try_into().unwrap();
	bounded_vec
}

pub fn set_block_number(n: u64) {
	System::set_block_number(n);
}
