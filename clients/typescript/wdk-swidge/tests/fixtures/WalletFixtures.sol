// Copyright 2026 PropellerHeads
// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.33;

/// Test-only token: permissionless minting, optional USDT-like approval reset.
contract FixtureToken {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    bool private requireReset;

    function setRequireReset(bool enabled) external { requireReset = enabled; }
    function mint(address recipient, uint256 value) external { balanceOf[recipient] += value; }

    function approve(address spender, uint256 value) external returns (bool) {
        require(!requireReset || value == 0 || allowance[msg.sender][spender] == 0, "reset required");
        allowance[msg.sender][spender] = value;
        return true;
    }

    function transferFrom(address owner, address recipient, uint256 value) external returns (bool) {
        require(allowance[owner][msg.sender] >= value, "insufficient allowance");
        require(balanceOf[owner] >= value, "insufficient balance");
        allowance[owner][msg.sender] -= value;
        balanceOf[owner] -= value;
        balanceOf[recipient] += value;
        return true;
    }
}

/// Test-only settlement harness with the supported outer ABI, not Tycho's router.
/// Its route bytes contain one uint256 output amount; it does not execute DEX routes.
contract FixtureRouter {
    address constant NATIVE = 0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE;
    struct ClientFeeParams {
        uint32 clientFeeBps;
        address clientFeeReceiver;
        uint256 maxClientContribution;
        uint256 deadline;
        bytes clientSignature;
    }

    function singleSwap(
        uint256 amountIn, address tokenIn, address tokenOut,
        uint256 expectedAmountOut, uint256 minAmountOut, address receiver,
        ClientFeeParams calldata, bytes calldata route
    ) external payable returns (uint256 amountOut) {
        amountOut = abi.decode(route, (uint256));
        require(amountIn > 0 && expectedAmountOut > 0 && minAmountOut > 0, "zero amount");
        require(amountOut >= minAmountOut, "minimum output");
        if (tokenIn == NATIVE) require(msg.value == amountIn, "native value");
        else {
            require(msg.value == 0, "unexpected value");
            require(FixtureToken(tokenIn).transferFrom(msg.sender, address(this), amountIn));
        }
        if (tokenOut == NATIVE) {
            (bool sent,) = receiver.call{value: amountOut}("");
            require(sent, "native output");
        } else FixtureToken(tokenOut).mint(receiver, amountOut);
    }
}
