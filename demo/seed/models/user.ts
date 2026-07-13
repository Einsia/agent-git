export interface User {
  id: number;
  email: string;
  user_id: string;
  createdAt: Date;
}

export function findUser(user_id: string): Promise<User> {
  return db.users.findOne({ user_id });
}
