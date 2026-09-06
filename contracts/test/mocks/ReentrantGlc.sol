// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

/// A hostile token that calls back into the bridge during a transfer, modelling
/// an ERC-777-style hook. Used to prove the token-moving entry points cannot be
/// re-entered.
contract ReentrantGlc is ERC20 {
    address public target;
    bytes public payload;
    bool public armed;
    bool public reentryAttempted;
    bool public reentrySucceeded;

    constructor() ERC20("Reentrant Goldcoin", "rGLC") {}

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }

    function arm(address target_, bytes calldata payload_) external {
        target = target_;
        payload = payload_;
        armed = true;
        reentryAttempted = false;
        reentrySucceeded = false;
    }

    function _update(address from, address to, uint256 value) internal override {
        super._update(from, to, value);
        if (armed && from != address(0) && to != address(0)) {
            armed = false;
            reentryAttempted = true;
            (bool ok,) = target.call(payload);
            reentrySucceeded = ok;
        }
    }
}
