// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.28;

/**
 * @notice A coin's trading fees, fixed at its launch.
 * @param curveFeeBps fee on each curve trade, in bps of its pToken side
 * @param curveCreatorShareBps part of the curve fee that goes to the creator side
 *        (the FeeVault), in bps of the fee; the rest goes to the platform
 * @param poolFee the Uniswap v4 LP fee of the coin's pool after graduation, in
 *        hundredths of a bip (10_000 = 1%)
 * @param poolCreatorShareBps part of the pool's fees that goes to the creator side
 */
struct Fees {
    uint16 curveFeeBps;
    uint16 curveCreatorShareBps;
    uint24 poolFee;
    uint16 poolCreatorShareBps;
}
