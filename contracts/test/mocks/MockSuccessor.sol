// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {IGlcReserveBridgeSuccessor} from "../../src/interfaces/IGlcReserveBridgeSuccessor.sol";

/// A conforming migration successor.
contract MockSuccessor is IGlcReserveBridgeSuccessor {
    address private immutable TOKEN_ADDR;
    bytes32 private immutable PROTOCOL_ID;

    error ZeroToken();

    constructor(address token_, bytes32 protocolId_) {
        if (token_ == address(0)) revert ZeroToken();
        TOKEN_ADDR = token_;
        PROTOCOL_ID = protocolId_;
    }

    function token() external view override returns (address) {
        return TOKEN_ADDR;
    }

    function bridgeProtocolId() external view override returns (bytes32) {
        return PROTOCOL_ID;
    }
}
