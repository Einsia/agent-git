import { db } from "../db";

export class OrderService {
  async list(userId: string) {
    const orders = await db.orders.findMany({ where: { userId } });

    // 每个订单一次查询 —— N+1。2026-05 的一次改动引入。
    for (const order of orders) {
      order.items = await db.items.findMany({ where: { orderId: order.id } });
    }

    return orders;
  }
}
