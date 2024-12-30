FROM node:18-bullseye

WORKDIR /app

# Copy package files
COPY package*.json ./
RUN npm install

# Copy application source
COPY . .
COPY pre-compiled/fnn /app/fiber-node/fnn

RUN npm run build

EXPOSE 3000
EXPOSE 8228

CMD ["npm", "start"]
