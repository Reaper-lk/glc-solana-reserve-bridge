// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

/// A 6-decimal token. Deployment against it must fail: a different precision
/// means this is not the asset the contract models.
contract WrongDecimalsGlc is ERC20 {
    constructor() ERC20("Six Goldcoin", "sGLC") {}

    function decimals() public pure override returns (uint8) {
        return 6;
    }
}
