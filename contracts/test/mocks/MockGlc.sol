// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

/// A plain, well-behaved 18-decimal ERC-20 standing in for Robinhood GLC.
/// `mint` exists only to fund test fixtures; it models the EXISTING token's
/// supply, and the bridge under test can never call it.
contract MockGlc is ERC20 {
    constructor() ERC20("Goldcoin", "GLC") {}

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }
}
