// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

/// @title Successor-bridge identification interface.
/// @notice The minimum surface `GlcRobinhoodBridge` interrogates on a proposed
///         migration successor before it will commit the full reserve to it.
///
/// This is deliberately tiny. Migration is guarded primarily by a 48-hour
/// delay and human verification; these calls exist to make the *obvious*
/// mistakes impossible (wrong token, wrong protocol family, an EOA, the
/// contract itself) rather than to prove the successor is correct. A larger
/// interface would couple this contract to a successor design that does not
/// exist yet, which is the coupling the Phase D brief warns against.
interface IGlcReserveBridgeSuccessor {
    /// @return The ERC-20 reserve token this bridge custodies. Must equal the
    ///         predecessor's token exactly, or the reserve would be moved into
    ///         a contract that cannot account for it.
    function token() external view returns (address);

    /// @return A constant naming the protocol FAMILY, shared by every version
    ///         of this bridge. Not a version identifier: a v2 successor is
    ///         expected to return the same value v1 does, which is what makes
    ///         it a usable check across an upgrade.
    function bridgeProtocolId() external view returns (bytes32);
}
