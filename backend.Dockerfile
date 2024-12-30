FROM node:18-alpine

WORKDIR /app

# Copy package files
COPY package*.json ./

RUN npm install

# Copy application source
COPY . .

RUN npm run build

EXPOSE 3000

CMD ["npm", "run", "start-backend"]
