// Copyright 2026 PropellerHeads
// SPDX-License-Identifier: Apache-2.0

import { SwidgeError } from '@tetherto/wdk-wallet/protocols'

/** A failed execution with enough progress to reconcile before trying again. */
export class FyndExecutionError extends SwidgeError {
  /**
   * @param {string} stage
   * @param {import('@tetherto/wdk-wallet/protocols').SwidgeTransaction[]} transactions
   * @param {boolean} submissionUnknown
   * @param {unknown} cause
   */
  constructor (stage, transactions, submissionUnknown, cause) {
    super(`Fynd execution stopped during ${stage}; inspect transaction progress before retrying.`, {
      reason: 'EXECUTION_STOPPED', cause
    })
    this.name = 'FyndExecutionError'
    this.stage = stage
    this.transactions = transactions.map(transaction => ({ ...transaction }))
    this.submissionUnknown = submissionUnknown
  }
}
