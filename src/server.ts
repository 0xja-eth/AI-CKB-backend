import dotenv from "dotenv";

dotenv.config();

import express from "express";
import { connect } from "./core/redis";
import baseRouter from "./routers";
import healthRouter from "./routers/health";
import fiberRouter from "./routers/fiber";

import morgan from "morgan";

const app = express();
app.use(express.json());

app.use(morgan("dev"));

// Mount routers
app.use("/", baseRouter);
app.use("/fiber", fiberRouter);
app.use("/health", healthRouter);

const PORT = process.env.PORT || 3000;
connect().then(() =>
  app.listen(PORT, () => {
    console.log(`Server is running on port ${PORT}`);
  })
);
