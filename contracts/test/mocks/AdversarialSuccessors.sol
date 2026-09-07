// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

/// Returns the correct token and protocol id but has no mechanism to ever move
/// what it receives. Exists to demonstrate, as a standing test, exactly how far
/// successor validation does NOT go.
contract BlackHoleSuccessor {
    address private immutable TOKEN_ADDR;
    bytes32 private immutable PROTOCOL_ID;

    constructor(address token_, bytes32 protocolId_) {
        TOKEN_ADDR = token_;
        PROTOCOL_ID = protocolId_;
    }

    function token() external view returns (address) {
        return TOKEN_ADDR;
    }

    function bridgeProtocolId() external view returns (bytes32) {
        return PROTOCOL_ID;
    }
}

/// Declares the same selectors NON-view and writes storage. The bridge calls
/// through a `view` interface, so solc emits STATICCALL and this must revert.
contract StateWritingSuccessor {
    address private immutable TOKEN_ADDR;
    bytes32 private immutable PROTOCOL_ID;
    uint256 public probeCounter;

    constructor(address token_, bytes32 protocolId_) {
        TOKEN_ADDR = token_;
        PROTOCOL_ID = protocolId_;
    }

    function token() external returns (address) {
        probeCounter += 1;
        return TOKEN_ADDR;
    }

    function bridgeProtocolId() external returns (bytes32) {
        probeCounter += 1;
        return PROTOCOL_ID;
    }
}
