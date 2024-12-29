
// Auth middleware
import {Request, Response} from "express";

const AIToken = process.env.AI_TOKEN as string;

export const authMiddleware = (req: Request, res: Response, next: Function) => {
  const auth = req.header("Authorization");
  if (auth !== AIToken)
    return res.status(401).json({ error: "Unauthorized" });
  next();
};