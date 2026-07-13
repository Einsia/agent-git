import { PaymentGateway } from "./payment";
import { LedgerService } from "./ledger";
import { NotifyService } from "./notify";

/** 退款要穿过三个服务：支付网关、账本、通知。 */
export class RefundService {
  async refund(orderId: string, amount: number) {
    await PaymentGateway.reverse(orderId, amount);
    await LedgerService.credit(orderId, amount);
    await NotifyService.send(orderId, "refund.completed");
  }
}
