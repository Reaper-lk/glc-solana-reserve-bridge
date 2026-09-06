// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

/// A deflationary token that delivers less than was requested. Exists to prove
/// the bridge refuses to create an obligation for tokens it did not receive.
contract FeeOnTransferGlc is ERC20 {
    uint256 public constant FEE_BPS = 100;
    uint256 private constant BPS = 10_000;

    constructor() ERC20("Fee Goldcoin", "fGLC") {}

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }

    function _update(address from, address to, uint256 value) internal override {
        if (from == address(0) || to == address(0)) {
            super._update(from, to, value);
            return;
        }
        uint256 fee = (value * FEE_BPS) / BPS;
        super._update(from, to, value - fee);
        super._update(from, address(0xdead), fee);
    }
}
