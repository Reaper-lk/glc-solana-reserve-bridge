// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

/// A contract that implements neither `token()` nor `bridgeProtocolId()`.
/// Committing a migration to it must revert rather than succeed silently.
contract NonConformingSuccessor {
    uint256 public something;

    function poke() external {
        something += 1;
    }
}
