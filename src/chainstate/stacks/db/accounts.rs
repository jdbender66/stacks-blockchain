// Copyright (C) 2013-2020 Blocstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::HashMap;

use rusqlite::types::ToSql;
use rusqlite::Row;

use burnchains::Address;

use chainstate::stacks::db::blocks::*;
use chainstate::stacks::db::*;
use chainstate::stacks::Error;
use chainstate::stacks::*;
use vm::clarity::{ClarityConnection, ClarityTransactionConnection};
use vm::database::marf::*;
use vm::database::*;
use vm::types::*;

use util::db::Error as db_error;
use util::db::*;

#[derive(Debug, Clone, PartialEq)]
pub struct MinerReward {
    pub address: StacksAddress,
    pub coinbase: u128,
    pub tx_fees_anchored: u128,
    pub tx_fees_streamed_produced: u128,
    pub tx_fees_streamed_confirmed: u128,
    pub vtxindex: u32, // will be 0 for the reward to the miner, and >0 for user burn supports
}

impl FromRow<MinerPaymentSchedule> for MinerPaymentSchedule {
    fn from_row<'a>(row: &'a Row) -> Result<MinerPaymentSchedule, db_error> {
        let address = StacksAddress::from_column(row, "address")?;
        let block_hash = BlockHeaderHash::from_column(row, "block_hash")?;
        let consensus_hash = ConsensusHash::from_column(row, "consensus_hash")?;
        let parent_block_hash = BlockHeaderHash::from_column(row, "parent_block_hash")?;
        let parent_consensus_hash = ConsensusHash::from_column(row, "parent_consensus_hash")?;

        let coinbase_text: String = row.get("coinbase");
        let tx_fees_anchored_text: String = row.get("tx_fees_anchored");
        let tx_fees_streamed_text: String = row.get("tx_fees_streamed");
        let burns_text: String = row.get("stx_burns");
        let burnchain_commit_burn = u64::from_column(row, "burnchain_commit_burn")?;
        let burnchain_sortition_burn = u64::from_column(row, "burnchain_sortition_burn")?;
        let fill_text: String = row.get("fill");
        let miner: bool = row.get("miner");
        let stacks_block_height = u64::from_column(row, "stacks_block_height")?;
        let vtxindex: u32 = row.get("vtxindex");

        let coinbase = coinbase_text
            .parse::<u128>()
            .map_err(|_e| db_error::ParseError)?;
        let tx_fees_anchored = tx_fees_anchored_text
            .parse::<u128>()
            .map_err(|_e| db_error::ParseError)?;
        let tx_fees_streamed = tx_fees_streamed_text
            .parse::<u128>()
            .map_err(|_e| db_error::ParseError)?;
        let stx_burns = burns_text
            .parse::<u128>()
            .map_err(|_e| db_error::ParseError)?;
        let fill = fill_text
            .parse::<u64>()
            .map_err(|_e| db_error::ParseError)?;

        let payment_data = MinerPaymentSchedule {
            address,
            block_hash,
            consensus_hash,
            parent_block_hash,
            parent_consensus_hash,
            coinbase,
            tx_fees_anchored,
            tx_fees_streamed,
            stx_burns,
            burnchain_commit_burn,
            burnchain_sortition_burn,
            fill,
            miner,
            stacks_block_height,
            vtxindex,
        };
        Ok(payment_data)
    }
}

impl MinerReward {
    pub fn total(&self) -> u128 {
        self.coinbase
            + self.tx_fees_anchored
            + self.tx_fees_streamed_produced
            + self.tx_fees_streamed_confirmed
    }
}

impl StacksChainState {
    pub fn get_account<T: ClarityConnection>(
        clarity_tx: &mut T,
        principal: &PrincipalData,
    ) -> StacksAccount {
        clarity_tx.with_clarity_db_readonly(|ref mut db| {
            let stx_balance = db.get_account_stx_balance(principal);
            let nonce = db.get_account_nonce(principal);
            StacksAccount {
                principal: principal.clone(),
                stx_balance,
                nonce,
            }
        })
    }

    pub fn get_account_ft<'a>(
        clarity_tx: &mut ClarityTx<'a>,
        contract_id: &QualifiedContractIdentifier,
        token_name: &str,
        principal: &PrincipalData,
    ) -> Result<u128, Error> {
        clarity_tx
            .connection()
            .with_clarity_db_readonly(|ref mut db| {
                let ft_balance = db.get_ft_balance(contract_id, token_name, principal)?;
                Ok(ft_balance)
            })
            .map_err(Error::ClarityError)
    }

    pub fn get_account_nft<'a>(
        clarity_tx: &mut ClarityTx<'a>,
        contract_id: &QualifiedContractIdentifier,
        token_name: &str,
        token_value: &Value,
    ) -> Result<PrincipalData, Error> {
        clarity_tx
            .connection()
            .with_clarity_db_readonly(|ref mut db| {
                let nft_owner = db.get_nft_owner(contract_id, token_name, token_value)?;
                Ok(nft_owner)
            })
            .map_err(Error::ClarityError)
    }

    /// Called each time a transaction is invoked from this principal, to e.g.
    /// debit the STX-denominated tx fee or transfer/burn STX.
    /// Will consolidate unlocked STX.
    /// DOES NOT UPDATE THE NONCE
    pub fn account_debit(
        clarity_tx: &mut ClarityTransactionConnection,
        principal: &PrincipalData,
        amount: u64,
    ) {
        clarity_tx
            .with_clarity_db(|ref mut db| {
                let mut snapshot = db.get_stx_balance_snapshot(principal);

                // last line of defense: if we don't have sufficient funds, panic.
                // This should be checked by the block validation logic.
                if !snapshot.can_transfer(amount as u128) {
                    panic!(
                        "Tried to debit {} from account {} (which only has {})",
                        amount,
                        principal,
                        snapshot.get_available_balance()
                    );
                }

                snapshot.debit(amount as u128);
                snapshot.save();
                Ok(())
            })
            .expect("FATAL: failed to debit account")
    }

    /// Called each time a transaction sends STX to this principal.
    /// No nonce update is needed, since the transfer action is not taken by the principal.
    pub fn account_credit(
        clarity_tx: &mut ClarityTransactionConnection,
        principal: &PrincipalData,
        amount: u64,
    ) {
        clarity_tx
            .with_clarity_db(|ref mut db| {
                let mut snapshot = db.get_stx_balance_snapshot(principal);
                snapshot.credit(amount as u128);

                let new_balance = snapshot.get_available_balance();
                snapshot.save();

                info!("{} credited: {} uSTX", principal, new_balance);
                Ok(())
            })
            .expect("FATAL: failed to credit account")
    }

    /// Called during the genesis / boot sequence.
    pub fn account_genesis_credit(
        clarity_tx: &mut ClarityTransactionConnection,
        principal: &PrincipalData,
        amount: u128,
    ) {
        clarity_tx
            .with_clarity_db(|ref mut db| {
                let mut balance = STXBalance::initial(amount);
                let mut snapshot = db.get_stx_balance_snapshot_genesis(principal);
                let existing_balance = snapshot.balance().amount_unlocked;
                if existing_balance > 0 {
                    balance.amount_unlocked = balance
                        .amount_unlocked
                        .checked_add(existing_balance)
                        .expect("Genesis credit balance overflow");
                }
                snapshot.set_balance(balance);
                snapshot.save();
                Ok(())
            })
            .expect("FATAL: failed to credit account")
    }

    /// Increment an account's nonce
    pub fn update_account_nonce(
        clarity_tx: &mut ClarityTransactionConnection,
        principal: &PrincipalData,
        cur_nonce: u64,
    ) {
        clarity_tx
            .with_clarity_db(|ref mut db| {
                let next_nonce = cur_nonce.checked_add(1).expect("OUT OF NONCES");
                db.set_account_nonce(&principal, next_nonce);
                Ok(())
            })
            .expect("FATAL: failed to set account nonce")
    }

    /// Lock up STX for PoX for a time.  Does NOT touch the account nonce.
    pub fn pox_lock(
        db: &mut ClarityDatabase,
        principal: &PrincipalData,
        lock_amount: u128,
        unlock_burn_height: u64,
    ) -> Result<(), Error> {
        assert!(unlock_burn_height > 0);
        assert!(lock_amount > 0);

        let mut snapshot = db.get_stx_balance_snapshot(principal);
        if snapshot.has_locked_tokens() {
            return Err(Error::PoxAlreadyLocked);
        }
        if !snapshot.can_transfer(lock_amount) {
            return Err(Error::PoxInsufficientBalance);
        }
        snapshot.lock_tokens(lock_amount, unlock_burn_height);

        debug!(
            "PoX lock {} uSTX (new balance {}) until burnchain block height {} for {:?}",
            snapshot.balance().amount_locked,
            snapshot.balance().amount_unlocked,
            unlock_burn_height,
            principal
        );

        snapshot.save();
        Ok(())
    }

    /// Schedule a miner payment in the future.
    /// Schedules payments out to both miners and users that support them.
    pub fn insert_miner_payment_schedule<'a>(
        tx: &mut StacksDBTx<'a>,
        block_reward: &MinerPaymentSchedule,
        user_burns: &Vec<StagingUserBurnSupport>,
    ) -> Result<(), Error> {
        assert!(block_reward.burnchain_commit_burn < i64::max_value() as u64);
        assert!(block_reward.burnchain_sortition_burn < i64::max_value() as u64);
        assert!(block_reward.stacks_block_height < i64::max_value() as u64);

        let index_block_hash = StacksBlockHeader::make_index_block_hash(
            &block_reward.consensus_hash,
            &block_reward.block_hash,
        );

        let args: &[&dyn ToSql] = &[
            &block_reward.address.to_string(),
            &block_reward.block_hash,
            &block_reward.consensus_hash,
            &block_reward.parent_block_hash,
            &block_reward.parent_consensus_hash,
            &format!("{}", block_reward.coinbase),
            &format!("{}", block_reward.tx_fees_anchored),
            &format!("{}", block_reward.tx_fees_streamed),
            &format!("{}", block_reward.stx_burns),
            &u64_to_sql(block_reward.burnchain_commit_burn)?,
            &u64_to_sql(block_reward.burnchain_sortition_burn)?,
            &format!("{}", block_reward.fill),
            &u64_to_sql(block_reward.stacks_block_height)?,
            &true,
            &0i64,
            &index_block_hash,
        ];

        tx.execute(
            "INSERT INTO payments (
                        address,
                        block_hash,
                        consensus_hash,
                        parent_block_hash,
                        parent_consensus_hash,
                        coinbase,
                        tx_fees_anchored,
                        tx_fees_streamed,
                        stx_burns,
                        burnchain_commit_burn,
                        burnchain_sortition_burn,
                        fill,
                        stacks_block_height,
                        miner,
                        vtxindex,
                        index_block_hash) \
                    VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            args,
        )
        .map_err(|e| Error::DBError(db_error::SqliteError(e)))?;

        for user_support in user_burns.iter() {
            assert!(user_support.burn_amount < i64::max_value() as u64);

            let args: &[&dyn ToSql] = &[
                &user_support.address.to_string(),
                &block_reward.block_hash,
                &block_reward.consensus_hash,
                &block_reward.parent_block_hash,
                &block_reward.parent_consensus_hash,
                &format!("{}", block_reward.coinbase),
                &"0".to_string(),
                &"0".to_string(),
                &"0".to_string(),
                &u64_to_sql(user_support.burn_amount)?,
                &u64_to_sql(block_reward.burnchain_sortition_burn)?,
                &format!("{}", block_reward.fill),
                &u64_to_sql(block_reward.stacks_block_height)?,
                &false,
                &user_support.vtxindex,
                &index_block_hash,
            ];

            tx.execute(
                "INSERT INTO payments (
                            address,
                            block_hash,
                            consensus_hash,
                            parent_block_hash,
                            parent_consensus_hash,
                            coinbase,
                            tx_fees_anchored,
                            tx_fees_streamed,
                            stx_burns,
                            burnchain_commit_burn,
                            burnchain_sortition_burn,
                            fill,
                            stacks_block_height,
                            miner,
                            vtxindex,
                            index_block_hash) \
                        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
                args,
            )
            .map_err(|e| Error::DBError(db_error::SqliteError(e)))?;
        }

        Ok(())
    }

    /// Find the reported poison-microblock data for this block
    /// Returns None if there are no forks.
    pub fn get_poison_microblock_report<T: ClarityConnection>(
        clarity_tx: &mut T,
        height: u64,
    ) -> Result<Option<(StacksAddress, u16)>, Error> {
        let principal_seq_opt = clarity_tx
            .with_clarity_db_readonly(|ref mut db| {
                Ok(db.get_microblock_poison_report(height as u32))
            })
            .map_err(Error::ClarityError)?;

        Ok(principal_seq_opt.map(|(principal, seq)| (principal.into(), seq)))
    }

    /// Get the scheduled miner rewards in a particular Stacks fork at a particular height.
    pub fn get_scheduled_block_rewards_in_fork_at_height<'a>(
        tx: &mut StacksDBTx<'a>,
        tip: &StacksHeaderInfo,
        block_height: u64,
    ) -> Result<Vec<MinerPaymentSchedule>, Error> {
        let ancestor_info = match StacksChainState::get_tip_ancestor(tx, tip, block_height)? {
            Some(info) => info,
            None => {
                test_debug!("No ancestor at height {}", block_height);
                return Ok(vec![]);
            }
        };

        let qry = "SELECT * FROM payments WHERE block_hash = ?1 AND consensus_hash = ?2 ORDER BY vtxindex ASC".to_string();
        let args: &[&dyn ToSql] = &[
            &ancestor_info.anchored_header.block_hash(),
            &ancestor_info.consensus_hash,
        ];
        let rows = query_rows::<MinerPaymentSchedule, _>(tx, &qry, args).map_err(Error::DBError)?;
        test_debug!(
            "{} rewards in {}/{}",
            rows.len(),
            &ancestor_info.consensus_hash,
            &ancestor_info.anchored_header.block_hash()
        );
        Ok(rows)
    }

    /// Get the scheduled miner rewards in a particular Stacks fork at a particular height.
    pub fn get_scheduled_block_rewards<'a>(
        tx: &mut StacksDBTx<'a>,
        tip: &StacksHeaderInfo,
    ) -> Result<Vec<MinerPaymentSchedule>, Error> {
        if tip.block_height < MINER_REWARD_MATURITY {
            return Ok(vec![]);
        }

        let block_height = tip.block_height - MINER_REWARD_MATURITY;
        StacksChainState::get_scheduled_block_rewards_in_fork_at_height(tx, tip, block_height)
    }

    /// Get the miner info at a particular burn/stacks block
    pub fn get_miner_info(
        conn: &DBConn,
        consensus_hash: &ConsensusHash,
        stacks_block_hash: &BlockHeaderHash,
    ) -> Result<Option<MinerPaymentSchedule>, Error> {
        let qry =
            "SELECT * FROM payments WHERE consensus_hash = ?1 AND block_hash = ?2 AND miner = 1"
                .to_string();
        let args = [
            consensus_hash as &dyn ToSql,
            stacks_block_hash as &dyn ToSql,
        ];
        let mut rows =
            query_rows::<MinerPaymentSchedule, _>(conn, &qry, &args).map_err(Error::DBError)?;
        let len = rows.len();
        match len {
            0 => {
                test_debug!(
                    "No miner information for {}/{}",
                    consensus_hash,
                    stacks_block_hash
                );
                Ok(None)
            }
            1 => Ok(rows.pop()),
            _ => {
                panic!(
                    "Multiple miners for {}/{}",
                    consensus_hash, stacks_block_hash
                );
            }
        }
    }

    /// What's the commission for reporting a poison microblock stream?
    fn poison_microblock_commission(coinbase: u128) -> u128 {
        (coinbase * POISON_MICROBLOCK_COMMISSION_FRACTION) / 100
    }

    /// Calculate a block mining participant's coinbase reward, given the block's miner and list of
    /// user-burn-supporters.
    ///
    /// If poison_reporter_opt is not None, then the returned MinerReward will reward the _poison reporter_,
    /// not the miner, for reporting the microblock stream fork.
    ///
    /// TODO: this is incomplete -- it does not calculate transaction fees.  This is just stubbed
    /// out for now -- it only grants miners and user burn supports their coinbases.
    ///
    fn calculate_miner_reward(
        mainnet: bool,
        participant: &MinerPaymentSchedule,
        miner: &MinerPaymentSchedule,
        users: &Vec<MinerPaymentSchedule>,
        poison_reporter_opt: Option<&StacksAddress>,
    ) -> MinerReward {
        ////////////////////// coinbase reward total /////////////////////////////////
        let (this_burn_total, other_burn_total) = {
            if participant.address == miner.address {
                // we're calculating the miner's reward
                let mut total_user: u128 = 0;
                for user_support in users.iter() {
                    total_user = total_user
                        .checked_add(user_support.burnchain_commit_burn as u128)
                        .expect("FATAL: user support burn overflow");
                }
                (participant.burnchain_commit_burn as u128, total_user)
            } else {
                // we're calculating a user burn support's reward
                let mut this_user: u128 = 0;
                let mut total_other: u128 = miner.burnchain_commit_burn as u128;
                for user_support in users.iter() {
                    if user_support.address != participant.address {
                        total_other = total_other
                            .checked_add(user_support.burnchain_commit_burn as u128)
                            .expect("FATAL: user support burn overflow");
                    } else {
                        this_user = user_support.burnchain_commit_burn as u128;
                    }
                }
                (this_user, total_other)
            }
        };

        let burn_total = other_burn_total
            .checked_add(this_burn_total)
            .expect("FATAL: combined burns exceed u128");

        test_debug!(
            "{}: Coinbase reward = {} * ({}/{})",
            participant.address.to_string(),
            participant.coinbase,
            this_burn_total,
            burn_total
        );

        // each participant gets a share of the coinbase proportional to the fraction it burned out
        // of all participants' burns.
        let coinbase_reward = participant
            .coinbase
            .checked_mul(this_burn_total as u128)
            .expect("FATAL: STX coinbase reward overflow")
            / (burn_total as u128);

        // process poison -- someone can steal a fraction of the total coinbase if they can present
        // evidence that the miner forked the microblock stream.  The remainder of the coinbase is
        // destroyed if this happens.
        let (recipient, coinbase_reward) = if let Some(reporter_address) = poison_reporter_opt {
            if participant.address == miner.address {
                // the poison-reporter, not the miner, gets a (fraction of the) reward
                debug!(
                    "{:?} will recieve poison-microblock commission {}",
                    &reporter_address.to_string(),
                    StacksChainState::poison_microblock_commission(coinbase_reward)
                );
                (
                    reporter_address.clone(),
                    StacksChainState::poison_microblock_commission(coinbase_reward),
                )
            } else {
                // users that helped a miner that reported a poison-microblock get nothing
                (StacksAddress::burn_address(mainnet), coinbase_reward)
            }
        } else {
            // no poison microblock reported
            (participant.address, coinbase_reward)
        };

        // TODO: missing transaction fee calculation
        let tx_fees_anchored = 0;
        let tx_fees_streamed_produced = 0;
        let tx_fees_streamed_confirmed = 0;
        debug!(
            "{}: {} coinbase, {} anchored fees, {} streamed fees, {} confirmed fees",
            &recipient.to_string(),
            coinbase_reward,
            tx_fees_anchored,
            tx_fees_streamed_produced,
            tx_fees_streamed_confirmed
        );

        // ("bad miner! no coinbase!")
        let miner_reward = MinerReward {
            address: recipient,
            coinbase: coinbase_reward,
            tx_fees_anchored: tx_fees_anchored,
            tx_fees_streamed_produced: tx_fees_streamed_produced,
            tx_fees_streamed_confirmed: tx_fees_streamed_confirmed,
            vtxindex: miner.vtxindex,
        };
        miner_reward
    }

    /// Find the latest miner reward to mature, assuming that there are mature rewards.
    /// Returns a list of payments to make to each address -- miners and user-support burners.
    pub fn find_mature_miner_rewards<'a>(
        clarity_tx: &mut ClarityTx<'a>,
        tip: &StacksHeaderInfo,
        mut latest_matured_miners: Vec<MinerPaymentSchedule>,
    ) -> Result<Option<(MinerReward, Vec<MinerReward>, MinerRewardInfo)>, Error> {
        let mainnet = clarity_tx.config.mainnet;
        if tip.block_height <= MINER_REWARD_MATURITY {
            // no mature rewards exist
            return Ok(None);
        }

        let reward_height = tip.block_height - MINER_REWARD_MATURITY;

        assert!(latest_matured_miners.len() > 0);
        assert!(latest_matured_miners[0].vtxindex == 0);
        assert!(latest_matured_miners[0].miner);

        let users = latest_matured_miners.split_off(1);
        let miner = latest_matured_miners
            .pop()
            .expect("BUG: no matured miners despite prior check");

        let reward_info = MinerRewardInfo {
            from_stacks_block_hash: miner.block_hash.clone(),
            from_block_consensus_hash: miner.consensus_hash.clone(),
        };

        // was this block penalized for mining a forked microblock stream?
        // If so, find the principal that detected the poison, and reward them instead.
        let poison_recipient_opt =
            StacksChainState::get_poison_microblock_report(clarity_tx, reward_height)?
                .map(|(reporter, _)| reporter);

        if let Some(ref _poison_reporter) = poison_recipient_opt.as_ref() {
            test_debug!(
                "Poison-microblock reporter {} at height {}",
                &_poison_reporter.to_string(),
                reward_height
            );
        } else {
            test_debug!("No poison-microblock report at height {}", reward_height);
        }

        // calculate miner reward
        let miner_reward = StacksChainState::calculate_miner_reward(
            mainnet,
            &miner,
            &miner,
            &users,
            poison_recipient_opt.as_ref(),
        );

        // calculate reward for each user-support-burn
        let mut user_rewards = vec![];
        for user_reward in users.iter() {
            let reward = StacksChainState::calculate_miner_reward(
                mainnet,
                user_reward,
                &miner,
                &users,
                poison_recipient_opt.as_ref(),
            );
            user_rewards.push(reward);
        }

        Ok(Some((miner_reward, user_rewards, reward_info)))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use burnchains::*;
    use chainstate::burn::*;
    use chainstate::stacks::db::test::*;
    use chainstate::stacks::index::*;
    use chainstate::stacks::Error;
    use chainstate::stacks::*;
    use util::hash::*;
    use vm::costs::ExecutionCost;

    fn make_dummy_miner_payment_schedule(
        addr: &StacksAddress,
        coinbase: u128,
        tx_fees_anchored: u128,
        tx_fees_streamed: u128,
        commit_burn: u64,
        sortition_burn: u64,
    ) -> MinerPaymentSchedule {
        MinerPaymentSchedule {
            address: addr.clone(),
            block_hash: FIRST_STACKS_BLOCK_HASH.clone(),
            consensus_hash: FIRST_BURNCHAIN_CONSENSUS_HASH.clone(),
            parent_block_hash: FIRST_STACKS_BLOCK_HASH.clone(),
            parent_consensus_hash: FIRST_BURNCHAIN_CONSENSUS_HASH.clone(),
            coinbase,
            tx_fees_anchored,
            tx_fees_streamed,
            stx_burns: 0,
            burnchain_commit_burn: commit_burn,
            burnchain_sortition_burn: sortition_burn,
            fill: 0xffffffffffffffff,
            miner: true,
            stacks_block_height: 0,
            vtxindex: 0,
        }
    }

    fn make_dummy_user_payment_schedule(
        addr: &StacksAddress,
        coinbase: u128,
        tx_fees_anchored: u128,
        tx_fees_streamed: u128,
        commit_burn: u64,
        sortition_burn: u64,
        vtxindex: u32,
    ) -> MinerPaymentSchedule {
        let mut sched = make_dummy_miner_payment_schedule(
            addr,
            coinbase,
            tx_fees_anchored,
            tx_fees_streamed,
            commit_burn,
            sortition_burn,
        );
        sched.miner = false;
        sched.vtxindex = vtxindex;
        sched
    }

    impl StagingUserBurnSupport {
        pub fn from_miner_payment_schedule(user: &MinerPaymentSchedule) -> StagingUserBurnSupport {
            StagingUserBurnSupport {
                consensus_hash: user.consensus_hash.clone(),
                anchored_block_hash: user.block_hash.clone(),
                address: user.address.clone(),
                burn_amount: user.burnchain_commit_burn,
                vtxindex: user.vtxindex,
            }
        }
    }

    fn advance_tip(
        chainstate: &mut StacksChainState,
        parent_header_info: &StacksHeaderInfo,
        block_reward: &mut MinerPaymentSchedule,
        user_burns: &mut Vec<StagingUserBurnSupport>,
    ) -> StacksHeaderInfo {
        let mut new_tip = parent_header_info.clone();

        new_tip.anchored_header.parent_block = parent_header_info.anchored_header.block_hash();
        new_tip.anchored_header.microblock_pubkey_hash =
            Hash160::from_data(&parent_header_info.anchored_header.microblock_pubkey_hash.0);
        new_tip.anchored_header.total_work.work =
            parent_header_info.anchored_header.total_work.work + 1;
        new_tip.microblock_tail = None;
        new_tip.block_height = parent_header_info.block_height + 1;
        new_tip.consensus_hash = ConsensusHash(
            Hash160::from_data(
                &Sha512Trunc256Sum::from_data(&parent_header_info.consensus_hash.0).0,
            )
            .0,
        );
        new_tip.burn_header_hash = BurnchainHeaderHash(
            Sha512Trunc256Sum::from_data(&parent_header_info.consensus_hash.0).0,
        );
        new_tip.burn_header_height = parent_header_info.burn_header_height + 1;
        new_tip.total_liquid_ustx = parent_header_info.total_liquid_ustx + block_reward.coinbase;

        block_reward.parent_consensus_hash = parent_header_info.consensus_hash.clone();
        block_reward.parent_block_hash = parent_header_info.anchored_header.block_hash().clone();
        block_reward.block_hash = new_tip.anchored_header.block_hash();
        block_reward.consensus_hash = new_tip.consensus_hash.clone();

        for ref mut user_burn in user_burns.iter_mut() {
            user_burn.anchored_block_hash = new_tip.anchored_header.block_hash();
            user_burn.consensus_hash = new_tip.consensus_hash.clone();
        }

        let mut tx = chainstate.index_tx_begin().unwrap();
        let tip = StacksChainState::advance_tip(
            &mut tx,
            &parent_header_info.anchored_header,
            &parent_header_info.consensus_hash,
            &new_tip.anchored_header,
            &new_tip.consensus_hash,
            &new_tip.burn_header_hash,
            new_tip.burn_header_height,
            new_tip.burn_header_timestamp,
            new_tip.microblock_tail.clone(),
            &block_reward,
            &user_burns,
            new_tip.total_liquid_ustx,
            &ExecutionCost::zero(),
            123,
        )
        .unwrap();
        tx.commit().unwrap();
        tip
    }

    #[test]
    fn get_tip_ancestor() {
        let mut chainstate = instantiate_chainstate(false, 0x80000000, "get_tip_ancestor_test");
        let miner_1 =
            StacksAddress::from_string(&"SP1A2K3ENNA6QQ7G8DVJXM24T6QMBDVS7D0TRTAR5".to_string())
                .unwrap();
        let user_1 =
            StacksAddress::from_string(&"SP2837ZMC89J40K4YTS64B00M7065C6X46JX6ARG0".to_string())
                .unwrap();
        let mut miner_reward = make_dummy_miner_payment_schedule(&miner_1, 500, 0, 0, 1000, 1000);
        let user_reward = make_dummy_user_payment_schedule(&user_1, 500, 0, 0, 750, 1000, 1);

        // dummy reward
        let mut tip_reward = make_dummy_miner_payment_schedule(
            &StacksAddress {
                version: 0,
                bytes: Hash160([0u8; 20]),
            },
            0,
            0,
            0,
            0,
            0,
        );

        let user_support = StagingUserBurnSupport::from_miner_payment_schedule(&user_reward);
        let mut user_supports = vec![user_support];

        {
            let mut tx = chainstate.index_tx_begin().unwrap();
            let ancestor_0 = StacksChainState::get_tip_ancestor(
                &mut tx,
                &StacksHeaderInfo::regtest_genesis(0),
                0,
            )
            .unwrap();
            assert!(ancestor_0.is_some());
        }

        let parent_tip = advance_tip(
            &mut chainstate,
            &StacksHeaderInfo::regtest_genesis(0),
            &mut miner_reward,
            &mut user_supports,
        );

        {
            let mut tx = chainstate.index_tx_begin().unwrap();
            let ancestor_0 = StacksChainState::get_tip_ancestor(&mut tx, &parent_tip, 0).unwrap();
            let ancestor_1 = StacksChainState::get_tip_ancestor(&mut tx, &parent_tip, 1).unwrap();

            assert!(ancestor_1.is_some());
            assert!(ancestor_0.is_some());
            assert_eq!(ancestor_0.unwrap().block_height, 0); // block 0 is the boot block
            assert_eq!(ancestor_1.unwrap().block_height, 1);
        }

        let tip = advance_tip(&mut chainstate, &parent_tip, &mut tip_reward, &mut vec![]);

        {
            let mut tx = chainstate.index_tx_begin().unwrap();
            let ancestor_2 = StacksChainState::get_tip_ancestor(&mut tx, &tip, 2).unwrap();
            let ancestor_1 = StacksChainState::get_tip_ancestor(&mut tx, &tip, 1).unwrap();
            let ancestor_0 = StacksChainState::get_tip_ancestor(&mut tx, &tip, 0).unwrap();

            assert!(ancestor_2.is_some());
            assert_eq!(ancestor_2.unwrap().block_height, 2);
            assert!(ancestor_1.is_some());
            assert_eq!(ancestor_1.unwrap().block_height, 1);
            assert!(ancestor_0.is_some());
            assert_eq!(ancestor_0.unwrap().block_height, 0); // block 0 is the boot block
        }
    }

    #[test]
    fn load_store_miner_payment_schedule() {
        let mut chainstate =
            instantiate_chainstate(false, 0x80000000, "load_store_miner_payment_schedule");
        let miner_1 =
            StacksAddress::from_string(&"SP1A2K3ENNA6QQ7G8DVJXM24T6QMBDVS7D0TRTAR5".to_string())
                .unwrap();
        let user_1 =
            StacksAddress::from_string(&"SP2837ZMC89J40K4YTS64B00M7065C6X46JX6ARG0".to_string())
                .unwrap();

        let mut miner_reward = make_dummy_miner_payment_schedule(&miner_1, 500, 0, 0, 1000, 1000);
        let user_reward = make_dummy_user_payment_schedule(&user_1, 500, 0, 0, 750, 1000, 1);

        let initial_tip = StacksHeaderInfo::regtest_genesis(0);

        let user_support = StagingUserBurnSupport::from_miner_payment_schedule(&user_reward);
        let mut user_supports = vec![user_support];

        let parent_tip = advance_tip(
            &mut chainstate,
            &StacksHeaderInfo::regtest_genesis(0),
            &mut miner_reward,
            &mut user_supports,
        );

        // dummy reward
        let mut tip_reward = make_dummy_miner_payment_schedule(
            &StacksAddress {
                version: 0,
                bytes: Hash160([0u8; 20]),
            },
            0,
            0,
            0,
            0,
            0,
        );
        let tip = advance_tip(&mut chainstate, &parent_tip, &mut tip_reward, &mut vec![]);

        {
            let mut tx = chainstate.index_tx_begin().unwrap();
            let payments_0 =
                StacksChainState::get_scheduled_block_rewards_in_fork_at_height(&mut tx, &tip, 0)
                    .unwrap();
            let payments_1 =
                StacksChainState::get_scheduled_block_rewards_in_fork_at_height(&mut tx, &tip, 1)
                    .unwrap();
            let payments_2 =
                StacksChainState::get_scheduled_block_rewards_in_fork_at_height(&mut tx, &tip, 2)
                    .unwrap();

            let mut expected_user_support = user_reward.clone();
            expected_user_support.consensus_hash = miner_reward.consensus_hash.clone();
            expected_user_support.parent_consensus_hash =
                miner_reward.parent_consensus_hash.clone();
            expected_user_support.block_hash = miner_reward.block_hash.clone();
            expected_user_support.parent_block_hash = miner_reward.parent_block_hash.clone();

            assert_eq!(payments_0, vec![]);
            assert_eq!(payments_1, vec![miner_reward, expected_user_support]);
            assert_eq!(payments_2, vec![tip_reward]);
        };
    }

    /*
    #[test]
    fn find_mature_miner_rewards() {
        let mut chainstate = instantiate_chainstate(false, 0x80000000, "find_mature_miner_rewards");
        let miner_1 =
            StacksAddress::from_string(&"SP1A2K3ENNA6QQ7G8DVJXM24T6QMBDVS7D0TRTAR5".to_string())
                .unwrap();
        let user_1 =
            StacksAddress::from_string(&"SP2837ZMC89J40K4YTS64B00M7065C6X46JX6ARG0".to_string())
                .unwrap();

        let mut parent_tip = StacksHeaderInfo::regtest_genesis(0);

        let mut matured_miners = (make_dummy_miner_payment_schedule(&miner_1, 0, 0, 0, 0, 0), vec![]);

        for i in 0..(MINER_REWARD_MATURITY + 1) {
            let mut miner_reward = make_dummy_miner_payment_schedule(
                &miner_1,
                500,
                (2 * i) as u128,
                (2 * i) as u128,
                1000,
                1000,
            );
            let user_reward = make_dummy_user_payment_schedule(
                &user_1,
                500,
                (2 * i) as u128,
                (2 * i) as u128,
                100,
                100,
                1,
            );
            let user_support = StagingUserBurnSupport::from_miner_payment_schedule(&user_reward);

            if i == 0 {
                matured_miners = (miner_reward.clone(), vec![user_reward.clone()]);
            }

            let mut user_supports = vec![user_support];

            let next_tip = advance_tip(
                &mut chainstate,
                &parent_tip,
                &mut miner_reward,
                &mut user_supports,
            );

            if i < MINER_REWARD_MATURITY {
                let mut tx = chainstate.chainstate_tx_begin().unwrap();
                let rewards_opt =
                    StacksChainState::find_mature_miner_rewards(&mut tx, &parent_tip, vec![])
                        .unwrap();
                assert!(rewards_opt.is_none()); // not mature yet
            }
            test_debug!("next tip = {:?}", &next_tip);
            parent_tip = next_tip;
        }

        let mut tx = chainstate.chainstate_tx_begin().unwrap().0;

        test_debug!("parent tip = {:?}", &parent_tip);
        let miner_reward = StacksChainState::calculate_miner_reward(
            false,
            &matured_miners.0,
            &matured_miners.0,
            &matured_miners.1,
            None
        );
        let mut user_rewards = vec![];
        for user_reward in matured_miners.1.iter() {
            let rw = StacksChainState::calculate_miner_reward(
                false,
                user_reward,
                &matured_miners.0,
                &matured_miners.1,
                None
            );
            user_rewards.push(rw);
        }

        let mut matured_rewards = vec![];
        matured_rewards.push(miner_reward.clone());
        matured_rewards.append(&mut user_rewards.clone());

        let rewards_opt =
            StacksChainState::find_mature_miner_rewards(&mut tx, &parent_tip, &matured_rewards).unwrap();
        assert!(rewards_opt.is_some());

        let rewards = rewards_opt.unwrap();
        assert_eq!(rewards.0, miner_reward);
        assert_eq!(rewards.1, user_rewards);
    }
    */

    #[test]
    fn miner_reward_one_miner_no_tx_fees_no_users() {
        let miner_1 =
            StacksAddress::from_string(&"SP1A2K3ENNA6QQ7G8DVJXM24T6QMBDVS7D0TRTAR5".to_string())
                .unwrap();
        let participant = make_dummy_miner_payment_schedule(&miner_1, 500, 0, 0, 1000, 1000);

        let miner_reward = StacksChainState::calculate_miner_reward(
            false,
            &participant,
            &participant,
            &vec![],
            None,
        );

        // miner should have received the entire coinbase
        assert_eq!(miner_reward.coinbase, 500);
        assert_eq!(miner_reward.tx_fees_anchored, 0);
        assert_eq!(miner_reward.tx_fees_streamed_produced, 0);
        assert_eq!(miner_reward.tx_fees_streamed_confirmed, 0);
    }

    #[test]
    fn miner_reward_one_miner_one_user_no_tx_fees() {
        let miner_1 =
            StacksAddress::from_string(&"SP1A2K3ENNA6QQ7G8DVJXM24T6QMBDVS7D0TRTAR5".to_string())
                .unwrap();
        let user_1 =
            StacksAddress::from_string(&"SP2837ZMC89J40K4YTS64B00M7065C6X46JX6ARG0".to_string())
                .unwrap();

        let miner = make_dummy_miner_payment_schedule(&miner_1, 500, 0, 0, 250, 1000);
        let user = make_dummy_user_payment_schedule(&user_1, 500, 0, 0, 750, 1000, 1);

        let reward_miner_1 = StacksChainState::calculate_miner_reward(
            false,
            &miner,
            &miner,
            &vec![user.clone()],
            None,
        );
        let reward_user_1 = StacksChainState::calculate_miner_reward(
            false,
            &user,
            &miner,
            &vec![user.clone()],
            None,
        );

        // miner should have received 1/4 the coinbase
        assert_eq!(reward_miner_1.coinbase, 125);
        assert_eq!(reward_miner_1.tx_fees_anchored, 0);
        assert_eq!(reward_miner_1.tx_fees_streamed_produced, 0);
        assert_eq!(reward_miner_1.tx_fees_streamed_confirmed, 0);

        // user should have received 3/4 the coinbase
        assert_eq!(reward_user_1.coinbase, 375);
        assert_eq!(reward_user_1.tx_fees_anchored, 0);
        assert_eq!(reward_user_1.tx_fees_streamed_produced, 0);
        assert_eq!(reward_user_1.tx_fees_streamed_confirmed, 0);
    }

    /*
    // TODO: broken; needs to be rewritten once transaction fee processing is added
    #[test]
    fn miner_reward_one_miner_one_user_anchored_tx_fees_unfull() {
        let mut sample = vec![];
        let miner_1 = StacksAddress::from_string(&"SP1A2K3ENNA6QQ7G8DVJXM24T6QMBDVS7D0TRTAR5".to_string()).unwrap();
        let user_1 = StacksAddress::from_string(&"SP2837ZMC89J40K4YTS64B00M7065C6X46JX6ARG0".to_string()).unwrap();
        let mut participant = make_dummy_miner_payment_schedule(&miner_1, 500, 100, 0, 250, 1000);
        let mut user = make_dummy_user_payment_schedule(&user_1, 500, 0, 0, 750, 1000, 1);

        // blocks are NOT full
        let fill_cutoff = (((MINER_FEE_MINIMUM_BLOCK_USAGE as u128) << 64) / 100u128) as u64;
        participant.fill = fill_cutoff - 1;
        user.fill = fill_cutoff - 1;

        sample.push((participant.clone(), vec![user.clone()]));

        for i in 0..9 {
            let mut next_participant = make_dummy_miner_payment_schedule(&miner_1, 250, 100, 0, 250, 1000);
            let mut next_user = make_dummy_user_payment_schedule(&user_1, 750, 0, 0, 750, 1000, 1);

            next_participant.fill = fill_cutoff - 1;
            next_user.fill = fill_cutoff - 1;

            sample.push((next_participant, vec![next_user]));
        }

        let reward_miner_1 = StacksChainState::calculate_miner_reward(&participant, &sample);
        let reward_user_1 = StacksChainState::calculate_miner_reward(&user, &sample);

        // miner should have received 1/4 the coinbase, and miner should have received only shared
        assert_eq!(reward_miner_1.coinbase, 125);
        assert_eq!(reward_miner_1.tx_fees_anchored, 1000);        // same miner, so they get everything
        assert_eq!(reward_miner_1.tx_fees_streamed_produced, 0);
        assert_eq!(reward_miner_1.tx_fees_streamed_confirmed, 0);

        // user should have received 3/4 the coinbase, but no tx fees
        assert_eq!(reward_user_1.coinbase, 375);
        assert_eq!(reward_user_1.tx_fees_anchored, 0);
        assert_eq!(reward_user_1.tx_fees_streamed_produced, 0);
        assert_eq!(reward_user_1.tx_fees_streamed_confirmed, 0);
    }

    // TODO: broken; needs to be rewritten once transaction fee processing is added
    #[test]
    fn miner_reward_two_miners_one_user_anchored_tx_fees_unfull() {
        let mut sample = vec![];
        let miner_1 = StacksAddress::from_string(&"SP1A2K3ENNA6QQ7G8DVJXM24T6QMBDVS7D0TRTAR5".to_string()).unwrap();
        let miner_2 = StacksAddress::from_string(&"SP8WWTGMNCCSB88QF4VYWN69PAMQRMF34FCT498G".to_string()).unwrap();
        let user_1 = StacksAddress::from_string(&"SP2837ZMC89J40K4YTS64B00M7065C6X46JX6ARG0".to_string()).unwrap();
        let mut participant_1 = make_dummy_miner_payment_schedule(&miner_1, 500, 100, 0, 250, 1000);
        let mut participant_2 = make_dummy_miner_payment_schedule(&miner_2, 500, 0, 0, 1000, 1000);
        let mut user = make_dummy_user_payment_schedule(&user_1, 500, 0, 0, 750, 1000, 1);

        // blocks are NOT full
        let fill_cutoff = (((MINER_FEE_MINIMUM_BLOCK_USAGE as u128) << 64) / 100u128) as u64;
        participant_1.fill = fill_cutoff - 1;
        participant_2.fill = fill_cutoff - 1;
        user.fill = fill_cutoff - 1;

        sample.push((participant_1.clone(), vec![user.clone()]));

        for i in 0..4 {
            let mut next_participant = make_dummy_miner_payment_schedule(&miner_1, 250, 100, 0, 250, 1000);
            let mut next_user = make_dummy_user_payment_schedule(&user_1, 750, 0, 0, 750, 1000, 1);

            next_participant.fill = fill_cutoff - 1;
            next_user.fill = fill_cutoff - 1;

            sample.push((next_participant, vec![next_user]));
        }

        for i in 0..5 {
            let mut next_participant = make_dummy_miner_payment_schedule(&miner_2, 250, 100, 0, 250, 1000);
            let mut next_user = make_dummy_user_payment_schedule(&user_1, 750, 0, 0, 750, 1000, 1);

            next_participant.fill = fill_cutoff - 1;
            next_user.fill = fill_cutoff - 1;

            sample.push((next_participant, vec![next_user]));
        }

        let reward_miner_1 = StacksChainState::calculate_miner_reward(&participant_1, &sample);
        let reward_miner_2 = StacksChainState::calculate_miner_reward(&participant_2, &sample);
        let reward_user_1 = StacksChainState::calculate_miner_reward(&user, &sample);

        // if miner 1 won, then it should have received 1/4 the coinbase, and miner should have received only shared tx fees
        assert_eq!(reward_miner_1.coinbase, 125);
        assert_eq!(reward_miner_1.tx_fees_anchored, 500);        // did half the work over the sample
        assert_eq!(reward_miner_1.tx_fees_streamed_produced, 0);
        assert_eq!(reward_miner_1.tx_fees_streamed_confirmed, 0);

        // miner 2 didn't mine this block
        assert_eq!(reward_miner_2.coinbase, 0);
        assert_eq!(reward_miner_2.tx_fees_anchored, 500);        // did half the work over the sample
        assert_eq!(reward_miner_2.tx_fees_streamed_produced, 0);
        assert_eq!(reward_miner_2.tx_fees_streamed_confirmed, 0);

        // user should have received 3/4 the coinbase, but no tx fees
        assert_eq!(reward_user_1.coinbase, 375);
        assert_eq!(reward_user_1.tx_fees_anchored, 0);
        assert_eq!(reward_user_1.tx_fees_streamed_produced, 0);
        assert_eq!(reward_user_1.tx_fees_streamed_confirmed, 0);
    }

    // TODO: broken; needs to be rewritten once transaction fee processing is added
    #[test]
    fn miner_reward_two_miners_one_user_anchored_tx_fees_full() {
        let mut sample = vec![];
        let miner_1 = StacksAddress::from_string(&"SP1A2K3ENNA6QQ7G8DVJXM24T6QMBDVS7D0TRTAR5".to_string()).unwrap();
        let miner_2 = StacksAddress::from_string(&"SP8WWTGMNCCSB88QF4VYWN69PAMQRMF34FCT498G".to_string()).unwrap();
        let user_1 = StacksAddress::from_string(&"SP2837ZMC89J40K4YTS64B00M7065C6X46JX6ARG0".to_string()).unwrap();
        let mut participant_1 = make_dummy_miner_payment_schedule(&miner_1, 500, 100, 0, 250, 1000);
        let mut participant_2 = make_dummy_miner_payment_schedule(&miner_2, 500, 0, 0, 1000, 1000);
        let mut user = make_dummy_user_payment_schedule(&user_1, 500, 0, 0, 750, 1000, 1);

        let fill_cutoff = ((MINER_FEE_MINIMUM_BLOCK_USAGE as u128) << 64) / 100u128;

        // blocks are full to 90%
        let fill = ((90u128 << 64) / (100u128)) as u64;
        participant_1.fill = fill;
        participant_2.fill = fill;
        user.fill = fill;

        sample.push((participant_1.clone(), vec![user.clone()]));

        for i in 0..4 {
            let mut next_participant = make_dummy_miner_payment_schedule(&miner_1, 250, 100, 0, 250, 1000);
            let mut next_user = make_dummy_user_payment_schedule(&user_1, 750, 0, 0, 750, 1000, 1);

            next_participant.fill = fill;
            next_user.fill = fill;

            sample.push((next_participant, vec![next_user]));
        }

        for i in 0..5 {
            let mut next_participant = make_dummy_miner_payment_schedule(&miner_2, 250, 100, 0, 250, 1000);
            let mut next_user = make_dummy_user_payment_schedule(&user_1, 750, 0, 0, 750, 1000, 1);

            next_participant.fill = fill;
            next_user.fill = fill;

            sample.push((next_participant, vec![next_user]));
        }

        let reward_miner_1 = StacksChainState::calculate_miner_reward(&participant_1, &sample);
        let reward_miner_2 = StacksChainState::calculate_miner_reward(&participant_2, &sample);
        let reward_user_1 = StacksChainState::calculate_miner_reward(&user, &sample);

        let expected_shared =
            if (500 * fill_cutoff) & 0x0000000000000000ffffffffffffffff > 0x00000000000000007fffffffffffffff {
                ((500 * fill_cutoff) >> 64) + 1
            }
            else {
                (500 * fill_cutoff) >> 64
            };

        // miner 1 should have received 1/4 the coinbase
        assert_eq!(reward_miner_1.coinbase, 125);
        assert_eq!(reward_miner_1.tx_fees_anchored, expected_shared);        // did half the work over the sample
        assert_eq!(reward_miner_1.tx_fees_streamed_produced, 0);
        assert_eq!(reward_miner_1.tx_fees_streamed_confirmed, 0);

        // miner 2 didn't win this block, so it gets no coinbase
        assert_eq!(reward_miner_2.coinbase, 0);
        assert_eq!(reward_miner_2.tx_fees_anchored, expected_shared);        // did half the work over the sample
        assert_eq!(reward_miner_2.tx_fees_streamed_produced, 0);
        assert_eq!(reward_miner_2.tx_fees_streamed_confirmed, 0);

        // user should have received 3/4 the coinbase, but no tx fees
        assert_eq!(reward_user_1.coinbase, 375);
        assert_eq!(reward_user_1.tx_fees_anchored, 0);
        assert_eq!(reward_user_1.tx_fees_streamed_produced, 0);
        assert_eq!(reward_user_1.tx_fees_streamed_confirmed, 0);
    }

    // TODO: broken; needs to be rewritten once transaction fee processing is added
    #[test]
    fn miner_reward_two_miners_one_user_anchored_tx_fees_full_streamed() {
        let mut sample = vec![];
        let miner_1 = StacksAddress::from_string(&"SP1A2K3ENNA6QQ7G8DVJXM24T6QMBDVS7D0TRTAR5".to_string()).unwrap();
        let miner_2 = StacksAddress::from_string(&"SP8WWTGMNCCSB88QF4VYWN69PAMQRMF34FCT498G".to_string()).unwrap();
        let user_1 = StacksAddress::from_string(&"SP2837ZMC89J40K4YTS64B00M7065C6X46JX6ARG0".to_string()).unwrap();
        let mut participant_1 = make_dummy_miner_payment_schedule(&miner_1, 500, 100, 100, 250, 1000);
        let mut participant_2 = make_dummy_miner_payment_schedule(&miner_2, 500, 0, 0, 1000, 1000);
        let mut user = make_dummy_user_payment_schedule(&user_1, 500, 0, 0, 750, 1000, 1);

        let fill_cutoff = ((MINER_FEE_MINIMUM_BLOCK_USAGE as u128) << 64) / 100u128;

        // blocks are full to 90%
        let fill = ((90u128 << 64) / (100u128)) as u64;
        participant_1.fill = fill;
        participant_2.fill = fill;
        user.fill = fill;

        sample.push((participant_1.clone(), vec![user.clone()]));

        for i in 0..4 {
            let mut next_participant = make_dummy_miner_payment_schedule(&miner_1, 250, 100, 100, 250, 1000);
            let mut next_user = make_dummy_user_payment_schedule(&user_1, 750, 0, 0, 750, 1000, 1);

            next_participant.fill = fill;
            next_user.fill = fill;

            sample.push((next_participant, vec![next_user]));
        }

        for i in 0..5 {
            let mut next_participant = make_dummy_miner_payment_schedule(&miner_2, 250, 100, 100, 250, 1000);
            let mut next_user = make_dummy_user_payment_schedule(&user_1, 750, 0, 0, 750, 1000, 1);

            next_participant.fill = fill;
            next_user.fill = fill;

            sample.push((next_participant, vec![next_user]));
        }

        let reward_miner_1 = StacksChainState::calculate_miner_reward(&participant_1, &sample);
        let reward_miner_2 = StacksChainState::calculate_miner_reward(&participant_2, &sample);
        let reward_user_1 = StacksChainState::calculate_miner_reward(&user, &sample);

        let expected_shared =
            if (500 * fill_cutoff) & 0x0000000000000000ffffffffffffffff > 0x00000000000000007fffffffffffffff {
                ((500 * fill_cutoff) >> 64) + 1
            }
            else {
                (500 * fill_cutoff) >> 64
            };

        // miner 1 produced this block with the help from users 3x as interested, so it gets 1/4 of the coinbase
        assert_eq!(reward_miner_1.coinbase, 125);
        assert_eq!(reward_miner_1.tx_fees_anchored, expected_shared);        // did half the work over the sample
        assert_eq!(reward_miner_1.tx_fees_streamed_produced, 240);                  // produced half of the microblocks, should get half the 2/5 of the reward
        assert_eq!(reward_miner_1.tx_fees_streamed_confirmed, 240);                 // confirmed half of the microblocks, should get half of the 3/5 reward

        // miner 2 didn't produce this block, so no coinbase
        assert_eq!(reward_miner_2.coinbase, 0);
        assert_eq!(reward_miner_2.tx_fees_anchored, expected_shared);        // did half the work over the sample
        assert_eq!(reward_miner_2.tx_fees_streamed_produced, 200);
        assert_eq!(reward_miner_2.tx_fees_streamed_confirmed, 300);

        // user should have received 3/4 the coinbase, but no tx fees
        assert_eq!(reward_user_1.coinbase, 375);
        assert_eq!(reward_user_1.tx_fees_anchored, 0);
        assert_eq!(reward_user_1.tx_fees_streamed_produced, 0);
        assert_eq!(reward_user_1.tx_fees_streamed_confirmed, 0);
    }
    */
}
