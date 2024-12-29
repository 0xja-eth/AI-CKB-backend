import dotenv from "dotenv";

dotenv.config();

import express from "express";
import { connect } from "./core/redis";
import baseRouter from "./routers";
import fiberRouter from "./routers/fiber";

const app = express();
app.use(express.json());

// Mount routers
app.use("/", baseRouter);
app.use("/fiber", fiberRouter);

const PORT = process.env.PORT || 3000;
connect().then(() =>
  app.listen(PORT, () => {
    console.log(`Server is running on port ${PORT}`);
  })
);
