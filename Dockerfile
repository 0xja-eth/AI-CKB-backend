FROM node:18-alpine

WORKDIR /app

COPY package.json package-lock.json ./

RUN npm install

COPY . .

RUN npm run build

EXPOSE 3000
EXPOSE 8228 # Fiber port

CMD ["npm", "start"]
