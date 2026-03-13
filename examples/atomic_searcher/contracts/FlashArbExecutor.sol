// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

interface IERC20 {
    function transfer(address to, uint256 amount) external returns (bool);
    function approve(address spender, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

interface IUniswapV2Pair {
    function swap(
        uint256 amount0Out,
        uint256 amount1Out,
        address to,
        bytes calldata data
    ) external;
    function token0() external view returns (address);
    function token1() external view returns (address);
}

interface IBalancerVault {
    function flashLoan(
        address recipient,
        address[] memory tokens,
        uint256[] memory amounts,
        bytes memory userData
    ) external;
}

/// @title FlashArbExecutor
/// @notice Executes cyclic arbitrage via flash loans with zero upfront capital.
///         Supports two tiers:
///         - Tier 1: UniV2/SushiV2 flash swap (near-zero extra gas)
///         - Tier 2: Balancer V2 flash loan (fallback for any route)
contract FlashArbExecutor {
    address public immutable owner;
    address public immutable balancerVault;

    // Two-phase callback guard: stores the address we expect to call us back.
    // Set before the external call, validated in the callback, cleared after.
    address private _expectedCaller;

    struct FlashSwapData {
        address receivedToken; // token we got from the pair (intermediate token)
        address repayToken;    // token we owe to the pair (source token, e.g. WETH)
        uint256 repayAmount;   // exact amount to repay (computed by Rust from reserves)
        address router;
        bytes routerCalldata;
    }

    event FlashArbExecuted(
        uint8 tier,
        address token,
        uint256 borrowed,
        uint256 repaid,
        uint256 profit
    );

    modifier onlyOwner() {
        require(msg.sender == owner, "not owner");
        _;
    }

    constructor(address _balancerVault) {
        owner = msg.sender;
        balancerVault = _balancerVault;
    }

    // ======================== Tier 1: UniV2 Flash Swap ========================

    /// @notice Execute arbitrage via UniV2/SushiV2 flash swap.
    /// @param pair The UniV2 pair to flash-swap from (first pool in cycle).
    /// @param amountOut Amount of output token to borrow from the pair.
    /// @param zeroForOne True if selling token0 for token1 (borrow token1).
    /// @param repayAmount Exact amount of input token to repay (computed from reserves).
    /// @param router TychoRouter address for remaining hops.
    /// @param routerCalldata Encoded calldata for hops 2..N.
    function executeFlashSwapV2(
        address pair,
        uint256 amountOut,
        bool zeroForOne,
        uint256 repayAmount,
        address router,
        bytes calldata routerCalldata
    ) external onlyOwner {
        _expectedCaller = pair;

        // Derive token addresses from pair
        address token0 = IUniswapV2Pair(pair).token0();
        address token1 = IUniswapV2Pair(pair).token1();

        // zeroForOne=true: input is token0, output (borrowed) is token1
        address receivedToken = zeroForOne ? token1 : token0;
        address repayToken = zeroForOne ? token0 : token1;
        uint256 amount0Out = zeroForOne ? 0 : amountOut;
        uint256 amount1Out = zeroForOne ? amountOut : 0;

        IUniswapV2Pair(pair).swap(
            amount0Out,
            amount1Out,
            address(this),
            abi.encode(FlashSwapData({
                receivedToken: receivedToken,
                repayToken: repayToken,
                repayAmount: repayAmount,
                router: router,
                routerCalldata: routerCalldata
            }))
        );

        _expectedCaller = address(0);
    }

    /// @notice UniV2/SushiV2 flash swap callback.
    function uniswapV2Call(
        address,
        uint256,
        uint256,
        bytes calldata data
    ) external {
        require(msg.sender == _expectedCaller, "unauthorized callback");

        FlashSwapData memory fd = abi.decode(data, (FlashSwapData));

        // 1. Send received tokens (intermediate token from pair) to the router
        uint256 receivedAmount = IERC20(fd.receivedToken).balanceOf(address(this));
        IERC20(fd.receivedToken).transfer(fd.router, receivedAmount);

        // 2. Execute remaining hops (2..N) via TychoRouter
        //    Router sends repayToken (e.g. WETH) back to this contract
        (bool success,) = fd.router.call(fd.routerCalldata);
        require(success, "router call failed");

        // 3. Repay the pair with the exact amount needed for K-invariant
        //    repayAmount is pre-computed by the Rust encoder from reserves:
        //    repayAmount = (amountOut * 1000 * reserveIn) / ((reserveOut - amountOut) * 997) + 1
        IERC20(fd.repayToken).transfer(msg.sender, fd.repayAmount);

        // 4. Sweep remaining profit to owner
        uint256 profit = IERC20(fd.repayToken).balanceOf(address(this));
        if (profit > 0) {
            IERC20(fd.repayToken).transfer(owner, profit);
        }

        emit FlashArbExecuted(1, fd.repayToken, receivedAmount, fd.repayAmount, profit);
    }

    // ======================== Tier 2: Balancer Flash Loan ========================

    /// @notice Execute arbitrage via Balancer V2 flash loan.
    /// @param token Token to flash-borrow (typically WETH).
    /// @param amount Amount to borrow.
    /// @param router TychoRouter address.
    /// @param routerCalldata Encoded calldata for ALL hops.
    function executeFlashLoan(
        address token,
        uint256 amount,
        address router,
        bytes calldata routerCalldata
    ) external onlyOwner {
        _expectedCaller = balancerVault;

        address[] memory tokens = new address[](1);
        tokens[0] = token;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = amount;

        IBalancerVault(balancerVault).flashLoan(
            address(this),
            tokens,
            amounts,
            abi.encode(router, routerCalldata)
        );

        _expectedCaller = address(0);
    }

    /// @notice Balancer V2 flash loan callback.
    function receiveFlashLoan(
        address[] memory tokens,
        uint256[] memory amounts,
        uint256[] memory feeAmounts,
        bytes memory userData
    ) external {
        require(msg.sender == _expectedCaller, "unauthorized callback");

        (address router, bytes memory routerCalldata) =
            abi.decode(userData, (address, bytes));

        address token = tokens[0];
        uint256 amount = amounts[0];
        uint256 fee = feeAmounts[0];

        // Approve router to pull tokens via transferFrom.
        // We must NOT pre-transfer: TychoRouter's arb-cycle check
        // (tokenIn == tokenOut) subtracts amountIn from receiver's initial
        // balance, which underflows if receiver holds 0.
        IERC20(token).approve(router, amount);

        // Execute all hops via TychoRouter (router pulls tokens itself)
        (bool success,) = router.call(routerCalldata);
        require(success, "router call failed");

        // Repay vault: borrowed amount + fee
        uint256 repayAmount = amount + fee;
        IERC20(token).transfer(balancerVault, repayAmount);

        // Sweep profit to owner
        uint256 profit = IERC20(token).balanceOf(address(this));
        if (profit > 0) {
            IERC20(token).transfer(owner, profit);
        }

        emit FlashArbExecuted(2, token, amount, repayAmount, profit);
    }

    // ======================== Emergency ========================

    /// @notice Rescue stuck ERC20 tokens.
    function rescueTokens(address token) external onlyOwner {
        uint256 balance = IERC20(token).balanceOf(address(this));
        if (balance > 0) {
            IERC20(token).transfer(owner, balance);
        }
    }

    /// @notice Rescue stuck native ETH.
    function rescueETH() external onlyOwner {
        uint256 balance = address(this).balance;
        if (balance > 0) {
            (bool success,) = owner.call{value: balance}("");
            require(success, "ETH transfer failed");
        }
    }

    receive() external payable {}
}
